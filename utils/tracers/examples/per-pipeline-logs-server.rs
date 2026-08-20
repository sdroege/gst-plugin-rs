/// See the documentation in per-pipeline-logs.rs
use std::{
    io::{BufReader, prelude::*},
    net::{TcpListener, TcpStream},
};

fn main() {
    let listener = TcpListener::bind("0.0.0.0:5555").unwrap();

    for stream in listener.incoming() {
        let stream = stream.unwrap();

        handle_connection(stream);
    }
}

#[derive(Default, Debug)]
struct LogLine {
    code_file: Option<String>,
    code_line: Option<u32>,
    level: Option<String>,
    function: Option<String>,
    object_name: Option<String>,
    category_name: Option<String>,
    message: Option<String>,
}

const NEW_LINE: u8 = b'\n';
const EQUALS: u8 = b'=';

#[derive(Default, Debug)]
enum ParserState {
    #[default]
    Initial,
    MultilineStart {
        data: Vec<u8>,
        key: String,
    },
    Multiline {
        data: Vec<u8>,
        key: String,
    },
}

#[derive(Default)]
struct Parser {
    state: ParserState,
    previous_byte: u8,
    accumulator: Vec<u8>,
    current_log_line: LogLine,
}

impl Parser {
    pub fn parse(&mut self, bytes: &[u8]) -> Vec<LogLine> {
        let mut log_lines = vec![];

        for byte in bytes {
            let byte = *byte;

            match self.state {
                ParserState::Initial => {
                    if byte == NEW_LINE {
                        if self.previous_byte == EQUALS {
                            if let Ok(s) = std::str::from_utf8(&self.accumulator)
                                && let Some((k, _)) = s.rsplit_once("=")
                            {
                                self.state = ParserState::MultilineStart {
                                    data: Vec::with_capacity(8),
                                    key: k.to_string(),
                                };
                            }
                            self.accumulator = Vec::with_capacity(1024);
                        } else {
                            if let Ok(s) = std::str::from_utf8(&self.accumulator)
                                && let Some((k, v)) = s.split_once("=")
                            {
                                match k {
                                    "CODE_FILE" => {
                                        self.current_log_line.code_file = Some(v.to_string());
                                    }
                                    "CODE_LINE" => {
                                        if let Ok(line) = v.parse::<u32>() {
                                            self.current_log_line.code_line = Some(line);
                                        }
                                    }
                                    "_LEVEL" => {
                                        self.current_log_line.level = Some(v.to_string());
                                    }
                                    "CODE_FUNC" => {
                                        self.current_log_line.function = Some(v.to_string());
                                    }
                                    "_OBJECT_NAME" => {
                                        self.current_log_line.object_name = Some(v.to_string());
                                    }
                                    "_CATEGORY_NAME" => {
                                        self.current_log_line.category_name = Some(v.to_string());
                                    }
                                    _ => (),
                                }
                            }
                            self.accumulator = Vec::with_capacity(1024);
                        }
                    } else {
                        self.accumulator.push(byte);
                    }
                }
                ParserState::MultilineStart {
                    ref mut data,
                    ref key,
                } => {
                    data.push(byte);
                    if data.len() == 8 {
                        let len = u64::from_le_bytes(data[0..8].try_into().unwrap());
                        self.state = ParserState::Multiline {
                            data: Vec::with_capacity(len as usize),
                            key: key.clone(),
                        }
                    }
                }
                ParserState::Multiline {
                    ref mut data,
                    ref key,
                } => {
                    data.push(byte);
                    if data.len() == data.capacity() {
                        if let Ok(s) = std::str::from_utf8(data)
                            && key == "MESSAGE"
                        {
                            self.current_log_line.message = Some(s.to_string());
                            let log_line = std::mem::take(&mut self.current_log_line);
                            log_lines.push(log_line);
                        }
                        self.state = ParserState::Initial;
                        self.previous_byte = 0;
                    }
                }
            }

            self.previous_byte = byte;
        }

        log_lines
    }
}

fn handle_connection(stream: TcpStream) {
    let mut parser = Parser::default();

    let mut reader = BufReader::new(stream);

    let path = format!(
        "{}-logs.txt",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    eprintln!("Client connected, writing logs to {}", path);

    let mut out_file = std::fs::File::create(path).unwrap();

    loop {
        let mut buffer = [0u8; 1024];
        match reader.read(&mut buffer) {
            Ok(len) => {
                if len == 0 {
                    eprintln!("EOF");
                    break;
                }

                for log_line in parser.parse(&buffer[..len]) {
                    let log_line = if let Some(object_name) = log_line.object_name {
                        format!(
                            "{}:{}: {} {} {} {}: {}\n",
                            log_line.code_file.as_deref().unwrap_or_default(),
                            log_line.code_line.unwrap_or_default(),
                            log_line.level.as_deref().unwrap_or_default(),
                            log_line.function.as_deref().unwrap_or_default(),
                            object_name,
                            log_line.category_name.as_deref().unwrap_or_default(),
                            log_line.message.as_deref().unwrap_or_default(),
                        )
                    } else {
                        format!(
                            "{}:{}: {} {} {}: {}\n",
                            log_line.code_file.as_deref().unwrap_or_default(),
                            log_line.code_line.unwrap_or_default(),
                            log_line.level.as_deref().unwrap_or_default(),
                            log_line.function.as_deref().unwrap_or_default(),
                            log_line.category_name.as_deref().unwrap_or_default(),
                            log_line.message.as_deref().unwrap_or_default(),
                        )
                    };

                    out_file.write_all(log_line.as_bytes()).unwrap();
                }
            }
            Err(err) => {
                eprintln!("Error reading: {err:?}");
                break;
            }
        }
    }
}
