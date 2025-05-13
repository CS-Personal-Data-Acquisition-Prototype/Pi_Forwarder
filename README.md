# Pi_Forwarder
A Rust crate for forwarding low-latency sensor data to locally connected clients along with an internal SQLite database used for batching the data to a remote server for long-term storage.

## Features
- Functions with recording rates of 100+ Hz
- Zero-copy data handling
- Parallel client broadcasting
- Low-latency websocket client connections
- Batched database synchonization
- Deletes local data on synchonization confirmation by utilizing transactions

## Overview
Pi_Forwarder recieves serial or networked sensor data; immediately forwarding that data by utilizing the rayon crate for parallelization, the Rust Bytes struct to allow each rayon thread to clone a reference to the underlying memory rather than the memory itself, along with low-latency websockets for real-time delivery to any connected clients; it then inserts the data into an internal SQLite database, which is periodically pulled from to batch transmit to a remote server via TCP.

## Setup
1. Open a terminal on the Raspberry Pi where the repository will be cloned

2. Install the Rust toolchain if not already installed
   - run the command
      ```bash
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
      ```
   - Hit enter again for the default installation
   - Check it was installed correctly by running the following commands
      ```bash
      rustc --version
      cargo --version
      ```

2. Clone the repository by running the following command
   ```git
      git clone https://github.com/CS-Personal-Data-Acquisition-Prototype/Pi_Forwarder.git
   ```

3. Add the configuration file `config.toml` in `src` directory
   - Follow the [Configuration](#configuration) section for format guidelines

4. Run the program by running the following command
   ```bash
   cargo run --release
   ```

## Configuration
The `config.toml` file contains the following headers and default values

```toml
[debug]
interval = 3_000                             # number of cycles to collect data before printing, when not running in release

[addrs]
local = "0.0.0.0:8080"                       # address to start local websocket server to listen for connections on
remote = "XX.X.X.XXX:7878"                   # address to send TCP batches to, with remote IP replacing the X
endpoint = "/sessions-sensors-data/batch"    # specific endpoint to send TCP batch requests to

[database]
file = "data_acquisition.db"                 # name of local database file

[batch]
interval = 10                                # time, in seconds, to wait between batches
count = 10_000                               # max number of rows to send in a single TCP batch

```

# License Notice
To apply the Apache License to your work, attach the following boilerplate notice. The text should be enclosed in the appropriate comment syntax for the file format. We also recommend that a file or class name and description of purpose be included on the same "printed page" as the copyright notice for easier identification within third-party archives.

    Copyright 2025 CS 462 Personal Data Acquisition Prototype Group
    
    Licensed under the Apache License, Version 2.0 (the "License"); you may not use this file except in compliance with the License. You may obtain a copy of the License at
    
    http://www.apache.org/licenses/LICENSE-2.0
    Unless required by applicable law or agreed to in writing, software distributed under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the License for the specific language governing permissions and limitations under the License.