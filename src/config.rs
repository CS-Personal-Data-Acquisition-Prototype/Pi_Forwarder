use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    pub debug: Debug,
    pub addrs: Addrs,
    pub database: Database,
    pub batch: Batch,
}

#[derive(Deserialize)]
pub struct Debug {
    pub interval: usize, // number of cycles to collect data before printing
}

#[derive(Deserialize)]
pub struct Addrs {
    pub local: String,    // address to listen for connections on
    pub remote: String,   // address to send TCP batches to
    pub endpoint: String, // specific endpoint to send batch requests to
}

#[derive(Deserialize)]
pub struct Database {
    pub file: String, // name of local database file
}

#[derive(Deserialize)]
pub struct Batch {
    pub interval: u64, // time in seconds to wait between batches
    pub count: usize,  // max number of rows to send in a single batch
}
