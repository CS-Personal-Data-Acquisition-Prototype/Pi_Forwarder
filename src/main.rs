use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::{
    fs::create_dir,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex, RwLock},
    thread::{self, sleep},
    time::{Duration, Instant},
};
use tungstenite::{accept, Bytes, Message, WebSocket};

mod config;
use config::Config;

// macro used to concatanate constants, allows values shared across constants to be pulled out
// incurs zero runtime and memory cost as its fully evaluated at compile-time and any intermediate values are removed
macro_rules! const_concat {
    ($($sub_str: expr),+ $(,)?) => {{
        // get number of parts and total length
        const PARTS: &[&str] = &[ $($sub_str),+ ];
        const LEN: usize = {
            let mut len = 0;
            let mut i = 0;
            while i < PARTS.len() {
                len += PARTS[i].len();
                i += 1;
            }
            len
        };

        // combine all parts into single byte array
        const fn combined() -> [u8; LEN] {
            let mut out = [0u8; LEN];
            let mut offset = 0;
            let mut i = 0;
            while i < PARTS.len() {
                let bytes = PARTS[i].as_bytes();
                let mut j = 0;
                while j < bytes.len() {
                    out[offset+j] = bytes[j];
                    j += 1;
                }
                offset += j;
                i += 1;
            }
            out
        }

        // convert from byte array to &str
        match std::str::from_utf8(&combined()) {
            Ok(s) => s,
            Err(_) => panic!("Failed to concatenate const strings with macro during compile."),
        }
    }};
}

// consts holding SQL queries
const TABLE_NAME: &str = "sensor_data";
const TABLE_QUERY: &str = const_concat!(
    "CREATE TABLE IF NOT EXISTS ",
    TABLE_NAME,
    " (id INTEGER PRIMARY KEY AUTOINCREMENT, data BLOB NOT NULL)"
);
const INSERT_QUERY: &str = const_concat!("INSERT INTO ", TABLE_NAME, " (data) VALUES (?1)");
const BATCH_QUERY: &str = const_concat!(
    "DELETE FROM ",
    TABLE_NAME,
    " WHERE id IN (SELECT id FROM ",
    TABLE_NAME,
    " LIMIT ?1) RETURNING *"
);
const BATCH_DELIM: &[u8] = b"<EOF>";
const SESSION_SENSOR_ID: usize = 1;

#[derive(Deserialize)]
struct DataPoint {
    timestamp: String,
    sensor_blob: serde_json::Value,
}

struct Forwarder {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    sensor_socket: Arc<Mutex<Option<WebSocket<TcpStream>>>>,
    sensor_connected: Arc<RwLock<bool>>,
    clients: Arc<RwLock<Vec<Mutex<WebSocket<TcpStream>>>>>,
    #[cfg(debug_assertions)]
    timing_samples: Vec<Duration>,
}

impl Forwarder {
    fn new(config: Config, mut source: PathBuf) -> Self {
        println!("Sqlite Version: {}", rusqlite::version());
        println!("num cpus: {}", num_cpus::get());

        // open a connection to the db and create the table
        source.push("database");
        if !source.exists() {
            if let Err(e) = create_dir(&source) {
                panic!("Failed to create database folder: {e}");
            }
        }
        source.push(&config.database.file);
        let sample_count = config.debug.interval;

        let fwd = Forwarder {
            config: Arc::new(config),
            db_path: Arc::new(source),
            sensor_socket: Arc::new(Mutex::new(None)),
            sensor_connected: Arc::new(RwLock::new(false)),
            clients: Arc::new(RwLock::new(Vec::new())),
            #[cfg(debug_assertions)]
            timing_samples: Vec::with_capacity(sample_count),
        };

        let db = fwd.get_db_conn();
        if let Err(_) = db.execute_batch(&format!("PRAGMA journal_mode=WAL; {TABLE_QUERY};")) {
            panic!("Failed to create table on database.");
        }
        let _ = db.close();

        fwd
    }

    // returns a connection to the DB file at the passed path
    fn get_db_conn(&self) -> Connection {
        match Connection::open(self.db_path.as_path()) {
            Ok(db) => db,
            Err(_) => panic!(
                "Failed to open database file at {:?}.",
                self.db_path.as_path()
            ),
        }
    }

    //TODO: could open table in memory and only but in db file when that fills?
    fn run(&mut self) {
        //start batch thread to TCP server
        println!("Starting batch thread");
        let batch_config = self.config.clone();
        let mut batch_db = self.get_db_conn();
        thread::spawn(move || {
            // get threads connection to DB
            let mut json_buffer = Vec::with_capacity(32 * batch_config.batch.count); // TODO: adjust based on actual data size
            loop {
                // send data once batch count is reached
                sleep(Duration::from_secs(batch_config.batch.interval));

                // start transaction with DB connection
                let tx = match batch_db.transaction() {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("Failed to start DB transaction: {e}");
                        continue;
                    }
                };

                // get row blobs as parsed structs
                println!("Getting batch to send");
                let batch = match tx.prepare(BATCH_QUERY) {
                    Ok(mut stmt) => {
                        let batch_count = batch_config.batch.count;
                        match stmt.query_map(params![batch_count], |row| {
                            row.get::<_, Vec<u8>>(1)
                                .map(|blob| match String::from_utf8(blob) {
                                    Ok(blob_str) => {
                                        match serde_json::from_str::<DataPoint>(&blob_str) {
                                            Ok(datapoint) => datapoint,
                                            Err(e) => {
                                                panic!("Failed to parse row into datapoint: {e}")
                                            }
                                        }
                                    }
                                    Err(e) => panic!("Expected only valid UTF-8 in blob data: {e}"),
                                })
                        }) {
                            Ok(r) => match r.collect::<Result<Vec<_>, _>>() {
                                Ok(b) => b,
                                Err(e) => {
                                    eprintln!("Failed to collect rows from query result: {e}");
                                    Vec::new()
                                }
                            },
                            Err(e) => {
                                eprintln!("Failed to execute batch query against DB: {e}");
                                Vec::new()
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to prepare statement: {}", e);
                        continue;
                    }
                };

                if !batch.is_empty() {
                    // format batch to server expectations
                    json_buffer.clear();
                    json_buffer.append(r#"{"datapoints":["#.as_bytes().to_vec().as_mut());

                    for (i, datapoint) in batch.iter().enumerate() {
                        if i > 0 {
                            json_buffer.push(',' as u8);
                        }
                        if let Err(e) = write!(
                            &mut json_buffer,
                            r#"{{"id":{},"datetime":{},data_blob:{}}}"#,
                            SESSION_SENSOR_ID,
                            datapoint.timestamp,
                            match serde_json::to_string(&datapoint.sensor_blob) {
                                Ok(blob_str) => blob_str,
                                Err(e) => panic!("Failed to parse data blob: {e}"),
                            }
                        ) {
                            panic!("Failed to write to json buffer: {e}")
                        }
                    }
                    let mut end_bytes: Vec<u8> = "]}{}".as_bytes().to_vec();
                    end_bytes.extend(BATCH_DELIM);
                    json_buffer.append(&mut end_bytes);

                    #[cfg(debug_assertions)]
                    println!("Batch parse buffer size: {}", &json_buffer.capacity());

                    // format TCP request
                    let request = format!(
                        "POST {} HTTP/1,1\r\n
                        Content-Type: application/json\r\n
                        Content-Length: {}\r\n\r\n
                        {}",
                        &batch_config.addrs.endpoint,
                        &json_buffer.len(),
                        String::from_utf8_lossy(&json_buffer),
                    );

                    // send data via TCP to remote server
                    println!("Sending batch");
                    if let Ok(mut stream) = TcpStream::connect(&batch_config.addrs.remote) {
                        if stream.write_all(request.as_bytes()).is_ok() {
                            // check for 204 response
                            let mut response = String::new();
                            let _ = stream.read_to_string(&mut response);

                            if response.contains("HTTP/1.1 204") {
                                if let Err(e) = tx.commit() {
                                    eprintln!("Failed to commit transaction to DB: {e}")
                                }
                            }
                        }
                    }
                }
                //tx and changes are dropped unless commit is ran in match statement
            }
        });

        // start client acceptor thread
        println!("Starting client acceptor thread");
        let acceptor_clients = self.clients.clone();
        let sensor_clone = self.sensor_socket.clone();
        let exists_clone = self.sensor_connected.clone();
        let acceptor_config = self.config.clone();
        thread::spawn(move || {
            let listener = match TcpListener::bind(&acceptor_config.addrs.local) {
                Ok(l) => {
                    println!("Server listening at {}", acceptor_config.addrs.local);
                    l
                }
                Err(_) => panic!(
                    "Failed to bind server at address {}.",
                    acceptor_config.addrs.local
                ),
            };

            // accept incoming connections into websockets
            for stream in listener.incoming() {
                let mut ws_stream = match stream {
                    Ok(s) => match accept(s) {
                        Ok(ws) => ws,
                        Err(e) => {
                            eprintln!("Failed to make ws handshake: {e}");
                            continue;
                        }
                    },
                    Err(e) => {
                        eprintln!("Incoming client connection failed: {e}");
                        continue;
                    }
                };

                // start thread to wait for initial message
                println!("Waiting for new client message");
                let client_list = acceptor_clients.clone();
                let sensor_ref = sensor_clone.clone();
                let exists_ref = exists_clone.clone();
                thread::spawn(move || {
                    match ws_stream.read() {
                        Ok(Message::Binary(data)) => {
                            // sensors send 'S' as first char in first message
                            if data.first() == Some(&b'S') {
                                // attempt to set socket as sensor connection
                                match sensor_ref.lock() {
                                    Ok(mut guard) => match *guard {
                                        Some(_) => {
                                            eprintln!("Rejecting connection while attempting to connect sensors when sensor connection is already established.");
                                            let _ = ws_stream.close(None);
                                        }
                                        None => {
                                            // set sensor socket and update connection bool to true
                                            *guard = Some(ws_stream);
                                            match exists_ref.write() {
                                                Ok(mut guard) => *guard = true,
                                                Err(e) => {
                                                    panic!("Sensor bool guard is poisoned: {e}")
                                                }
                                            };
                                        }
                                    },
                                    Err(e) => panic!("Sensor guard is poisoned: {e}"),
                                }
                                println!("Sensors connected");
                                // return so socket isn't added to client list as well
                                return;
                            }

                            // if not the sensors, then add to client list
                            match client_list.write() {
                                Ok(mut guard) => guard.push(Mutex::new(ws_stream)),
                                Err(e) => panic!("Client guard is poisoned: {e}"),
                            };
                            println!("Client connected");
                        }
                        Err(e) => eprintln!("Failed to get message from connection: {e}"),
                        _ => eprintln!("Unexpected message format encountered."),
                    };
                });
            }
        });

        // start data storage thread
        let (data_tx, data_rx) = mpsc::channel::<Bytes>();
        let db_conn = self.get_db_conn();
        thread::spawn(move || {
            // get threads connection to DB
            loop {
                match data_rx.recv() {
                    //TODO: ensure data.to_vec() works as expected
                    Ok(data) => {
                        if let Err(e) = db_conn.execute(INSERT_QUERY, params![data.to_vec()]) {
                            panic!("Failed to execute INSERT_QUERY with DB connection: {e}")
                        }
                    }
                    Err(e) => panic!("Failed to recieve DB data from the channel: {e}"),
                };
            }
        });

        // start handling data on main thread
        loop {
            // continue loop if sensors aren't connected
            match self.sensor_connected.read() {
                Ok(guard) => {
                    if !*guard {
                        continue;
                    }
                }
                Err(e) => panic!("Sensor bool guard is poisoned: {e}"),
            };

            //attempt to get sensor mutex lock
            let mut guard = match self.sensor_socket.lock() {
                Ok(guard) => guard,
                Err(e) => panic!("Sensor connection is poisoned: {e}"),
            };

            // attempt to read from sensor socket
            match guard.as_mut() {
                //  if somehow no socket, drop lock and wait short time
                None => {
                    drop(guard);
                    sleep(Duration::from_millis(self.config.batch.interval / 2));
                    continue;
                }
                Some(socket) => match socket.read() {
                    // data is in Bytes struct which is cheaply cloneable and all point to the same underlying memory
                    Ok(Message::Binary(data)) => {
                        #[cfg(debug_assertions)]
                        let start = Instant::now();

                        // broadcast data to all clients
                        self.broadcast(data.clone());

                        // send data to be stored in DB
                        if let Err(e) = data_tx.send(data) {
                            panic!("Failed to send data to the DB: {e}")
                        }

                        // check timing if a debug build
                        #[cfg(debug_assertions)]
                        {
                            self.timing_samples.push(start.elapsed());
                            if self.timing_samples.len() >= self.config.debug.interval {
                                let (min, max, sum) = self
                                    .timing_samples
                                    .par_iter()
                                    .fold(
                                        || (Duration::MAX, Duration::ZERO, Duration::ZERO),
                                        |(min, max, sum), &d| (min.min(d), max.max(d), sum + d),
                                    )
                                    .reduce(
                                        || (Duration::MAX, Duration::ZERO, Duration::ZERO),
                                        |(min_a, max_a, sum_a), (min_b, max_b, sum_b)| {
                                            (min_a.min(min_b), max_a.max(max_b), sum_a + sum_b)
                                        },
                                    );
                                println!(
                                    "{} cycles took {} total seconds. Min: {}, Max: {}, Avg: {}",
                                    self.config.debug.interval,
                                    sum.as_secs(),
                                    min.as_millis(),
                                    max.as_millis(),
                                    sum.as_millis() as usize / self.config.debug.interval
                                );
                                self.timing_samples.clear();
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Sensors disconnected: {e}");
                        match self.sensor_connected.write() {
                            Ok(mut bool_guard) => {
                                *bool_guard = false;
                                *guard = None;
                            }
                            Err(e) => panic!("Sensor connected guard is poisoned: {e}"),
                        }
                    }
                    _ => {
                        eprintln!("Recieved unexpected message type from sensors.");
                        continue;
                    }
                },
            };
        }
    }

    fn broadcast(&self, data: Bytes) {
        let binary_message = tungstenite::Message::Binary(data);
        match self.clients.read() {
            Ok(g) => g.par_iter().for_each(|conn_guard| match conn_guard.lock() {
                Ok(mut conn) => {
                    if let Err(e) = conn.send(binary_message.clone()) {
                        eprintln!("Failed to send message to client: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("Socket guard is poisoned, closing socket: {e}");
                }
            }),
            Err(e) => panic!("Client guard is poisoned: {e}"),
        };
    }
}

fn main() {
    match std::env::current_dir() {
        Ok(mut path) => {
            path.push("src");
            let source = path.clone();
            path.push("config.toml");
            match std::fs::read_to_string(&path) {
                Ok(config_str) => match toml::from_str::<Config>(&config_str) {
                    Ok(toml_config) => Forwarder::new(toml_config, source).run(),
                    Err(e) => panic!(
                        "Failed to parse file 'config.toml' at {:?} as valid TOML: {e}",
                        path
                    ),
                },
                Err(e) => panic!(
                    "Failed to open configuration file 'config.toml' at {:?}: {e}",
                    path
                ),
            }
        }
        Err(e) => panic!("Failed to get current directory: {e}"),
    }
}
