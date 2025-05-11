use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use rusqlite::{params, Connection};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Arc, Mutex, RwLock},
    thread::{self, sleep},
    time::{Duration, Instant},
};
use tungstenite::{accept, Bytes, Message, WebSocket};

//macro used to concatanate constants, allows values shared across constants to be pulled out
//incurs zero runtime and memory cost as its fully evaluated at compile-time and any intermediate values are removed
macro_rules! const_concat {
    ($($sub_str: expr),+ $(,)?) => {{
        //get number of parts and total length
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

        //convert from byte array to &str
        match std::str::from_utf8(&combined()) {
            Ok(s) => s,
            Err(_) => panic!("Failed to concatenate const strings with macro during compile."),
        }
    }};
}

//converts an int to static str
//can use on smaller values without runtime or memory overhead using 'n as u128'
const fn u128_to_str(n: u128) -> &'static str {
    if n == 0 {
        return "0";
    }

    // write digits in reverse
    const MAX_DIGITS: usize = 39; // u189 max is 39 digits (340,282,366,920,938,463,463,374,607,431,768,211,455)
    let mut buf = [0u8; MAX_DIGITS];
    let mut i = 0;
    let mut num = n;
    while num > 0 {
        buf[(MAX_DIGITS - 1) - i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }

    // trim to size
    let mut out = [0u8; MAX_DIGITS];
    let mut j = 0;
    while j < i {
        out[j] = buf[MAX_DIGITS - i + j];
        j += 1;
    }

    // convert bytes to &str then to static str
    match std::str::from_utf8(&out) {
        // this is safe as only ASCII 0-9 is written and we are in const which is compile-time
        Ok(s) => unsafe { std::mem::transmute::<&str, &'static str>(s) },
        Err(_) => panic!("Failed to convert u128 to static str with macro during compile."),
    }
}

const TARGET_INTERVAL: Duration = Duration::from_millis(10); //100hz
const STATS_INTERVAL: usize = 3000;

// local address to listen for connections on
const LOCAL_SERVER_ADDR: &str = "0.0.0.0:7878";
// address to send TCP batches to
const SERVER_ADDR: &str = "";
// specific endpoint to send batch requests to
const BATCH_ENDPOINT: &str = "";
const DB_PATH: &str = "../src/data_acquisition.db";

const TABLE_NAME: &str = "sensor_data";
const TABLE_QUERY: &str = const_concat!(
    "CREATE TABLE IF NOT EXISTS ",
    TABLE_NAME,
    " (id INTEGER PRIMARY KEY AUTOINCREMENT, data BLOB NOT NULL)"
);
const INSERT_QUERY: &str = const_concat!(
    "INSERT INTO ",
    TABLE_NAME,
    " (data) VALUES (?1)"
);
const BATCH_QUERY: &str = const_concat!("DELETE FROM ", TABLE_NAME, " LIMIT ?1 RETURNING *");
// time in seconds to wait between batches
const BATCH_INTERVAL: u64 = 10;
// number of rows to send in a batch
const BATCH_COUNT: u32 = 10000;

struct Forwarder {
    sensor_socket: Arc<Mutex<Option<WebSocket<TcpStream>>>>,
    sensor_connected: Arc<RwLock<bool>>,
    clients: Arc<RwLock<Vec<Mutex<WebSocket<TcpStream>>>>>,
    #[cfg(debug_assertions)]
    timing_samples: Vec<Duration>,
}

impl Forwarder {
    fn new() -> Self {
        //TODO: could open table in memory and only but in db file when that fills?
        let db = Self::get_db_conn();
        if let Err(_) = db.execute_batch(TABLE_QUERY) {
            panic!("Failed to create table on database.");
        }
        let _ = db.close();

        println!("num cpus: {}", num_cpus::get());
        Forwarder {
            sensor_socket: Arc::new(Mutex::new(None)),
            sensor_connected: Arc::new(RwLock::new(false)),
            clients: Arc::new(RwLock::new(Vec::new())),
            #[cfg(debug_assertions)]
            timing_samples: Vec::with_capacity(STATS_INTERVAL),
        }
    }

    fn get_db_conn() -> Connection {
        match Connection::open(DB_PATH) {
            Ok(db) => db,
            Err(_) => panic!("Failed to open database file at {DB_PATH}."),
        }
    }

    fn run(&mut self) {
        //start batch thread to TCP server
        thread::spawn(move || {
            // get threads connection to DB
            let mut batch_db = Forwarder::get_db_conn();
            loop {
                // send data once batch count is reached
                sleep(Duration::from_secs(BATCH_INTERVAL));

                // start transaction with DB connection
                let tx = match batch_db.transaction() {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("Failed to start DB transaction: {e}");
                        continue;
                    }
                };

                // //get row blobs as vectors of u8 to batch send
                let batch = match tx.prepare(BATCH_QUERY) {
                    Ok(mut stmt) => match stmt.query_map(params![BATCH_COUNT], |row| row.get::<_, Vec<u8>>(0)) {
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
                    },
                    Err(e) => {
                        eprintln!("Failed to prepare statement: {}", e);
                        continue;
                    }
                };

                if !batch.is_empty() {
                    // format batch to server expectations
                    let json = "";
                    let len = json.len();

                    // format request
                    let request = format!(
                        "POST {BATCH_ENDPOINT} HTTP/1,1\r\n
                        Content-Type: application/json\r\n
                        Content-Length: {len}\r\n\r\n
                        {json}"
                    );

                    // send via TCP
                    if let Ok(mut stream) = TcpStream::connect(SERVER_ADDR) {
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
        let acceptor_clients = self.clients.clone();
        let sensor_clone = self.sensor_socket.clone();
        let exists_clone = self.sensor_connected.clone();
        thread::spawn(move || {
            let listener = match TcpListener::bind(LOCAL_SERVER_ADDR) {
                Ok(l) => l,
                Err(_) => panic!("Failed to bind server at address {LOCAL_SERVER_ADDR}."),
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
                                // return so socket isn't added to client list as well
                                return;
                            }

                            // if not the sensors, then add to client list
                            match client_list.write() {
                                Ok(mut guard) => guard.push(Mutex::new(ws_stream)),
                                Err(e) => panic!("Client guard is poisoned: {e}"),
                            }
                        }
                        Err(e) => eprintln!("Failed to get message from connection: {e}"),
                        _ => eprintln!("Unexpected message format encountered."),
                    };
                });
            }
        });

        // start data storage thread
        let (data_tx, data_rx) = mpsc::channel::<Bytes>();
        thread::spawn(move || {
            // get threads connection to DB
            let db_conn = Forwarder::get_db_conn();
            loop {
                match data_rx.recv() {
                    //TODO: ensure data.to_vec() works as expected
                    Ok(data) => if let Err(e) = db_conn.execute(INSERT_QUERY, params![data.to_vec()]) {
                        panic!("Failed to execute INSERT_QUERY with DB connection: {e}")
                    },
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

            // attempt to read from sensor socket
            match self.sensor_socket.lock() {
                Ok(mut guard) => match guard.as_mut() {
                    //  if somehow no socket, drop lock and wait short time
                    None => {
                        drop(guard);
                        sleep(TARGET_INTERVAL / 2);
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
                                if self.timing_samples.len() >= STATS_INTERVAL {
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
                                        "{STATS_INTERVAL} cycles took {} total seconds. Min: {}, Max: {}, Avg: {}",
                                        sum.as_secs(),
                                        min.as_millis(),
                                        max.as_millis(),
                                        sum.as_millis() as usize/STATS_INTERVAL
                                    );
                                    self.timing_samples.clear();
                                }
                            }
                        }
                        //TODO: handle possible sensor disconnections here
                        Err(e) => eprintln!("Sensors may have disconnected: {e}"),
                        _ => {
                            eprintln!("Recieved unexpected message type from sensors.");
                            continue;
                        }
                    },
                },
                Err(e) => panic!("Sensor connection is poisoned: {e}"),
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
    Forwarder::new().run();
}
