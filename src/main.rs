use bytes::BytesMut;
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
const TARGET_INTERVAL: Duration = Duration::from_millis(10); // 100 hz
const SPIN_TIME: Duration = Duration::from_micros(500);
const NULL_DATA_BYTES: Bytes = Bytes::from_static(b"null");

const SESSION_SENSOR_ID: usize = 1; // TODO: Remove hardcoded value

#[derive(Deserialize)]
struct DataPoint {
    timestamp: String,
    sensor_blob: serde_json::Value,
}

struct Sensor {
    port: Box<dyn serialport::SerialPort + 'static>,
    name: String,
    offset: usize, // position in sensor buffer
    size: usize,   // expected data size
}

struct Forwarder {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    sensor_socket: Arc<Mutex<Option<WebSocket<TcpStream>>>>,
    sensor_connected: Arc<RwLock<bool>>,
    clients: Arc<RwLock<Vec<Mutex<WebSocket<TcpStream>>>>>,
    serial_sensors: Vec<Sensor>,
    #[cfg(debug_assertions)]
    timing_samples: Vec<Duration>,
}

impl Forwarder {
    fn new(config: Config, mut source: PathBuf) -> Self {
        println!("Sqlite Version: {}", rusqlite::version());

        // open a connection to the db and create the table
        source.push("database");
        if !source.exists() {
            if let Err(e) = create_dir(&source) {
                panic!("Failed to create database folder: {e}");
            }
        }
        source.push(&config.database.file);
        let sample_count = config.debug.interval;

        // set up serial connection to sensors if in config
        let serial_sensors = if config.addrs.sensors.is_empty() {
            vec![]
        } else {
            let mut sensors: Vec<Sensor> = vec![];
            #[cfg(debug_assertions)]
            {
                // print all avaliable ports
                let ports = match serialport::available_ports() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("Falied to read avaliable ports: {e}");
                        Vec::new()
                    }
                };
                ports
                    .iter()
                    .for_each(|port| println!("Name: {}", port.port_name));
            }

            // connect to each sensor after validating their info
            let mut offset: usize = 0;
            config.addrs.sensors.iter().for_each(|val| {
                match val.as_array() {
                    Some(arr) => {
                        let serial_port = match arr[0].as_str() {
                            Some(port) => Some(port),
                            None => {
                                eprintln!("Failed to parse a sensor serial port as a string.");
                                None
                            }
                        };
                        let baud_rate = match arr[1].as_integer() {
                            Some(rate) => Some(rate as u32),
                            None => {
                                eprintln!("Failed to parse a sensor baud rate as an integer.");
                                None
                            }
                        };
                        let name = match arr[2].as_str() {
                            Some(name) => Some(name),
                            None => {
                                eprintln!("Failed to parse a sensor name as a str.");
                                None
                            }
                        };
                        let data_size = match arr[3].as_integer() {
                            Some(size) => Some(size as usize),
                            None => {
                                eprintln!("Failed to parse a sensor data size as an integer.");
                                None
                            }
                        };

                        // open the port and push it to the vec
                        if serial_port.is_some()
                            && baud_rate.is_some()
                            && name.is_some()
                            && data_size.is_some()
                        {
                            println!(
                                "Sensor port: {}, baud rate: {}, name: {}, size: {}",
                                serial_port.unwrap(),
                                baud_rate.unwrap(),
                                name.unwrap(),
                                data_size.unwrap(),
                            );
                            match serialport::new(serial_port.unwrap(), baud_rate.unwrap())
                                .timeout(Duration::from_millis(1))
                                .open()
                            {
                                Ok(port) => {
                                    sensors.push(Sensor {
                                        port,
                                        name: name.unwrap().to_string(),
                                        offset,
                                        size: data_size.unwrap(),
                                    });
                                    offset += data_size.unwrap();
                                }
                                Err(e) => eprintln!(
                                    "Failed to open port {} with baud rate {}: {e}",
                                    serial_port.unwrap(),
                                    baud_rate.unwrap()
                                ),
                            }
                        }
                    }
                    None => {
                        eprintln!("Failed to parse one of the sensor configurations as an array.")
                    }
                }
            });
            sensors
        };

        let fwd = Forwarder {
            config: Arc::new(config),
            db_path: Arc::new(source),
            sensor_socket: Arc::new(Mutex::new(None)),
            sensor_connected: Arc::new(RwLock::new(false)),
            clients: Arc::new(RwLock::new(Vec::new())),
            serial_sensors,
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
            let mut json_buffer = vec![0; 32 * batch_config.batch.count]; // TODO: adjust based on actual data size
            loop {
                // send data once batch count is reached
                sleep(Duration::from_secs(batch_config.batch.interval));

                //TODO: if bool false, ping server to check connection and save to bool, only batch if bool is true, set bool to false if batch fails

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
                            r#"{{"id":{},"datetime":"{}","data_blob":{}}}"#,
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
                    json_buffer.append("]}".as_bytes().to_vec().as_mut());

                    #[cfg(debug_assertions)]
                    println!("Batch parse buffer size: {}", &json_buffer.capacity());

                    // format TCP request
                    let request = format!(
                        "POST {} HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        &batch_config.addrs.endpoint,
                        &json_buffer.len(),
                        String::from_utf8_lossy(&json_buffer),
                    );

                    // send data via TCP to remote server
                    match TcpStream::connect(&batch_config.addrs.remote) {
                        Ok(mut stream) => {
                            println!("Sending batch");
                            if let Err(e) = stream.write_all(request.as_bytes()) {
                                eprintln!("Failed to write batch to stream: {e}")
                            } else {
                                if let Err(e) = stream.flush() {
                                    eprintln!("Failed to flush the stream: {e}")
                                }
                                println!("Waiting for remote response");
                                // check for 204 response
                                let mut response = vec![0; 128];
                                if let Err(e) = stream.read(&mut response) {
                                    eprintln!("Failed to read from stream: {e}")
                                }

                                if response.starts_with(b"HTTP/1.1 204") {
                                    if let Err(e) = tx.commit() {
                                        eprintln!("Failed to commit transaction to DB: {e}")
                                    } else {
                                        println!("Batch successful");
                                    }
                                } else {
                                    eprintln!("Recieved unexpected response: {:?}", response)
                                }
                            }
                        }
                        Err(e) => eprintln!(
                            "Failed to connect to remote server at {:?}: {e}",
                            &batch_config.addrs.remote
                        ),
                    }
                } else {
                    println!("Nothing to batch");
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
        let mut next_interval = Instant::now();
        let mut serial_buffer =
            BytesMut::with_capacity(self.serial_sensors.iter().map(|sensor| sensor.size).sum());
        loop {
            #[cfg(debug_assertions)]
            let start = Instant::now();

            let data: Result<Bytes, String> = match self.serial_sensors.is_empty() {
                // get data from serial ports into json format
                false => {
                    let mut result = Ok(());
                    serial_buffer.fill(0);

                    // check if all sensors have data
                    for sensor in &self.serial_sensors {
                        match sensor.port.bytes_to_read() {
                            Ok(n) => {
                                if n <= 0 {
                                    result = Err(format!(
                                        "Port '{}' has no data avaliable, reported: {n}",
                                        sensor.name
                                    ));
                                    break;
                                }
                            }
                            Err(e) => {
                                result = Err(format!(
                                    "Failed to check bytes on port '{}': {e}",
                                    sensor.name
                                ));
                                break;
                            }
                        }
                    }

                    if result.is_err() {
                        Err(result.unwrap_err())
                    } else {
                        // read all data from sensors into respective slices
                        for sensor in &mut self.serial_sensors {
                            let buffer_slice =
                                &mut serial_buffer[sensor.offset..sensor.offset + sensor.size];
                            match sensor.port.read(buffer_slice) {
                                //TODO: make sure read doesn't go past the buffer slice bounds
                                Ok(n) => {
                                    println!("Read '{}' bytes from sensor '{}'", n, sensor.name)
                                }
                                Err(e) => {
                                    result = Err(format!(
                                        "Failed to read from sensor '{}': {e}",
                                        sensor.name
                                    ));
                                    break;
                                }
                            };
                        }

                        if result.is_err() {
                            Err(result.unwrap_err())
                        } else {
                            let bytes = serial_buffer.clone().freeze();

                            let mut parts = vec![Vec::new(); self.serial_sensors.len()];
                            self.serial_sensors
                                .iter()
                                .enumerate()
                                .for_each(|(i, sensor)| {
                                    parts[i] = bytes[sensor.offset..sensor.offset + sensor.size]
                                        .split(|&b| b == b',')
                                        .map(|slice| Bytes::copy_from_slice(slice))
                                        .collect::<Vec<Bytes>>();
                                });

                            println!(
                                "Timestamp: {}",
                                chrono::Utc::now()
                                    .format("%Y-%m-%d %H:%M:%S%.9f")
                                    .to_string()
                            );

                            let get_part = |i: usize, j: usize| -> Bytes {
                                parts
                                    .get(i)
                                    .and_then(|bytes| bytes.get(j))
                                    .cloned()
                                    .unwrap_or(NULL_DATA_BYTES.clone())
                            };

                            let mut json_bytes = BytesMut::with_capacity(1_024); //TODO: replace with more accurate capacity
                            json_bytes.extend_from_slice(b"{\"timestamp\": \"");
                            json_bytes.extend_from_slice(
                                chrono::Utc::now()
                                    .format("%Y-%m-%d %H:%M:%S%.9f")
                                    .to_string()
                                    .as_bytes(),
                            );

                            json_bytes.extend_from_slice(b"\", \"sensor_blob\": {\"lat\": ");
                            json_bytes.extend_from_slice(&NULL_DATA_BYTES); // lat

                            json_bytes.extend_from_slice(b", \"lon\": ");
                            json_bytes.extend_from_slice(&NULL_DATA_BYTES); // lon

                            json_bytes.extend_from_slice(b", \"accel_x\": ");
                            json_bytes.extend_from_slice(&get_part(2, 0)); // accel_x

                            json_bytes.extend_from_slice(b", \"accel_y\": ");
                            json_bytes.extend_from_slice(&get_part(2, 1)); // accel_y

                            json_bytes.extend_from_slice(b", \"accel_z\": ");
                            json_bytes.extend_from_slice(&get_part(2, 2)); // accel_z

                            json_bytes.extend_from_slice(b", \"gyro_x\": ");
                            json_bytes.extend_from_slice(&get_part(2, 3)); // gyro_x

                            json_bytes.extend_from_slice(b", \"gyro_y\": ");
                            json_bytes.extend_from_slice(&get_part(2, 4)); // gyro_y

                            json_bytes.extend_from_slice(b", \"gyro_z\": ");
                            json_bytes.extend_from_slice(&get_part(2, 5)); // gyro_z

                            json_bytes.extend_from_slice(b", \"mag_x\": ");
                            json_bytes.extend_from_slice(&NULL_DATA_BYTES); // mag_x

                            json_bytes.extend_from_slice(b", \"mag_y\": ");
                            json_bytes.extend_from_slice(&NULL_DATA_BYTES); // mag_y

                            json_bytes.extend_from_slice(b", \"mag_z\": ");
                            json_bytes.extend_from_slice(&NULL_DATA_BYTES); // mag_z

                            json_bytes.extend_from_slice(b", \"force\": ");
                            json_bytes.extend_from_slice(&get_part(0, 0)); // force / dac 2

                            json_bytes.extend_from_slice(b", \"linear\": ");
                            json_bytes.extend_from_slice(&get_part(1, 0)); // linear / dac 0 (or 1)

                            json_bytes.extend_from_slice(b", \"string\": ");
                            json_bytes.extend_from_slice(&get_part(1, 1)); // linear / dac 1 (or 0)
                            json_bytes.extend_from_slice(b"}}");

                            println!(
                                "Serial recieved:\n{}",
                                String::from_utf8(json_bytes.to_vec())
                                    .unwrap_or("Error parsing json bytes to utf8".to_string())
                            );

                            Ok(json_bytes.freeze())
                        }
                    }
                }
                // attempt to read from a websockets sensor connection
                true => {
                    let mut result = Ok(());
                    // check if sensors are connected
                    match self.sensor_connected.read() {
                        Ok(guard) => {
                            if !*guard {
                                result = Err("No sensors are connected".to_string());
                                continue; //TODO: remove continue
                            }
                        }
                        Err(e) => panic!("Sensor bool guard is poisoned: {e}"),
                    };

                    if result.is_err() {
                        Err(result.unwrap_err())
                    } else {
                        //attempt to get sensor mutex lock
                        let mut guard = match self.sensor_socket.lock() {
                            Ok(guard) => guard,
                            Err(e) => panic!("Sensor connection is poisoned: {e}"),
                        };

                        // attempt to read from sensor socket
                        let data = match guard.as_mut() {
                            //  if somehow no socket, drop lock
                            None => {
                                //drop(guard);
                                //sleep(Duration::from_millis(self.config.batch.interval / 2));
                                result = Err("Expected socket connection is missing".to_string());
                                None
                            }
                            Some(socket) => match socket.read() {
                                // data is in Bytes struct which is cheaply cloneable and all point to the same underlying memory
                                Ok(Message::Binary(data)) => Some(data),
                                Err(e) => {
                                    match self.sensor_connected.write() {
                                        Ok(mut bool_guard) => {
                                            *bool_guard = false;
                                            *guard = None;
                                        }
                                        Err(e) => panic!("Sensor connected guard is poisoned: {e}"),
                                    }
                                    result = Err(format!("Sensors disconnected: {e}"));
                                    None
                                }
                                _ => {
                                    result = Err(format!(
                                        "Recieved unexpected message type from sensors."
                                    ));
                                    None
                                }
                            },
                        };

                        if result.is_err() {
                            Err(result.unwrap_err())
                        } else {
                            Ok(data.unwrap())
                        }
                    }
                }
            };

            match data {
                Ok(data) => {
                    // broadcast data to all clients
                    self.broadcast(data.clone());

                    // send data to be stored in DB
                    if let Err(e) = data_tx.send(data) {
                        panic!("Failed to send data to the DB: {e}")
                    }
                }
                Err(e) => eprintln!("Failed to recieve data: {e}"),
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

            let now = Instant::now();
            if now >= next_interval {
                next_interval = now + TARGET_INTERVAL;
            } else if now + SPIN_TIME < next_interval {
                sleep(next_interval - SPIN_TIME - now);
            }

            while Instant::now() < next_interval {
                std::hint::spin_loop();
            }

            next_interval += TARGET_INTERVAL;
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
