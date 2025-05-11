# Pi_Transmit

A Rust utility for transmitting database data from a Raspberry Pi to a remote server automatically.

## Overview

Pi_Transmit is designed to facilitate the seamless transfer of database information from a Raspberry Pi device to a designated server. This tool is particularly useful for IoT applications, remote monitoring systems, and data collection scenarios where periodic data uploads are required.

## Features

- Automated database synchronization
- Configurable transfer schedules
- Secure data transmission
- Error handling and retry mechanisms
- Persistent connection management

## Setup

1. Clone this repository to your Raspberry Pi
2. Install Rust and Cargo if not already installed
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```
3. Configure the `config.ini` file (see Configuration section)
4. Build and run the project using Cargo

## Configuration

The `config.ini` file contains essential settings for the application. Place this file in the `src` directory with the following sections:

```ini
[server]
ip = 0.0.0.0        ; IP address of the remote server
port = 9000               ; Port number for the connection
max_retries = 3           ; Number of retry attempts on connection failure
retry_delay = 2           ; Delay (in seconds) between retry attempts

[connection]
persistent = true         ; Whether to maintain a persistent connection
keep_alive_interval = 30  ; Interval (in seconds) for keep-alive signals
connection_timeout = 60   ; Connection timeout in seconds
auto_reconnect = true     ; Automatically reconnect if connection drops

[transmission]
continuous = true         ; Whether to transmit data continuously
transmit_interval = 1.0   ; Time between transmissions in seconds
buffer_size = 4096        ; Buffer size for data transmission
batch_size = 100          ; Number of records to transmit in one batch

[database]
path = /home/pi/Pi_TCP/src/data_acquisition.db  ; Path to the SQLite database file
```

## Usage

To build and run the application:

```bash
cargo build --release
cargo run --release
```

For running as a background service, you can create a systemd service file or use:

```bash
nohup cargo run --release > output.log &
```

## Troubleshooting

Check the log output for detailed information about any errors encountered during transmission. Common issues include:
- Network connectivity problems
- Database access errors
- Server connection failures
- Transmission interruptions

## License

Copyright 2025 CS 462 Personal Data Acquisition Prototype Group

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0
