#![deny(rust_2018_idioms)]
#![deny(rust_2021_compatibility)]
#![deny(missing_docs)]

//! lbuffer - line-based buffering/reblocking for pipes
//!
//! lbuffer is a utility like buffer/mbuffer which reblocks I/O. It's like a
//! rubber band for your pipes. However, unlike buffer/mbuffer, it buffers
//! lines, instead of fixed-size blocks.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::io;
use std::io::Stdin;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::thread;
use std::result::Result as StdResult;
use std::cmp::max;

use anyhow::anyhow;
use anyhow::Context as ErrContext;
use anyhow::Result;

use clap::Parser;
use clap::CommandFactory;

use crossbeam::channel::bounded;
use crossbeam::channel::Sender;
use crossbeam::channel::Receiver;
use crossbeam::queue::ArrayQueue;

use smashquote::unescape_bytes;

#[allow(unused_macros)]
macro_rules! println {
    ($($rest:tt)*) => {
        std::compile_error!("Don't use println")
    }
}

#[derive(Parser, Debug)]
#[clap(author, version)]
/// line-based buffering/reblocking for your pipes
///
/// lbuffer is a utility like buffer/mbuffer which reblocks I/O. It's like a
/// rubber band for your pipes. However, unlike buffer/mbuffer, it buffers
/// lines, instead of fixed-size blocks.
struct Args {
    /// Input files
    ///
    /// All files are opened immediately when lbuffer starts
    #[clap(name="FILE", parse(from_os_str))]
    files: Vec<OsString>,

    /// Number of lines to buffer
    #[clap(short = 'b', long, default_value_t = 1024usize)]
    buffer_lines: usize,

    /// Begin writing when we have at least this many lines in the buffer
    #[clap(long, default_value_t = 1usize)]
    high_wm: usize,
    
    /// Begin reading when we have at most this many lines in the buffer
    #[clap(long, default_value_t = 1023usize)]
    low_wm: usize,
    
    /// Allows lbuffer to clobber output files
    #[clap(short='f', long)]
    clobber: bool,

    /// Line delimiter
    #[clap(short='d', long, parse(from_os_str), default_value = "\n")]
    delimiter: OsString,
}

impl Args {
    fn validate(&self) -> () {
        if self.high_wm >= self.buffer_lines {
            Args::command().error(
                clap::ErrorKind::ArgumentConflict,
                "high-wm must be less than buffer-lines",
            )
            .exit();
        }
        if self.low_wm >= self.buffer_lines {
            Args::command().error(
                clap::ErrorKind::ArgumentConflict,
                "low-wm must be less than buffer-lines",
            )
            .exit();
        }
        for p in &self.files {
            if p == &OsString::from("-") {
                eprintln!("WARNING: lbuffer does not treat - as stdin/stdout.");
                eprintln!("         use /dev/stdin /dev/stdout instead.");
            }
        }
        
    }
}

enum InFileish {
    F(File),
    S(Stdin),
}

impl Read for InFileish {
    fn read(
        &mut self,
        buf: &mut [u8],
    ) -> Result<usize, io::Error> {
        match self {
            Self::F(f) => f.read(buf),
            Self::S(f) => f.read(buf),
        }
    }
}

enum Message {
    Line(Vec<u8>),
    End(Result<()>),
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    for offset in 0usize..(haystack.len()-needle.len()) {
        if &haystack[offset..(offset+needle.len())] == needle {
            return Some(offset);
        }
    }
    return None;
}

fn reader(
    args: &'static Args,
    full_sender: Sender<Message>,
    empty_receiver: Receiver<Message>,
    delimiter: Vec<u8>,
) -> Result<()> {
    let stdin_h = std::io::stdin();
    let mut si = stdin_h.lock();
    const PAGE: usize = 4096;
    let mut hold_buf: Option<Vec<u8>> = None;
    loop {
        let message: Message = match empty_receiver.recv() {
            StdResult::Ok(m) => m,
            StdResult::Err(e) => { 
                return Result::Err(e).with_context(||
                    format!("Bug: Main thread hung up on reader.")
                );
            }
        };
        let mut next_buf: Vec<u8> = match message {
            Message::Line(v) => v,
            Message::End(res) => match res {
                Ok(_) => {
                    return Err(anyhow!("Main thread asked us to stop reading."));
                }
                Err(e) => {
                    return Err(e).with_context(||
                        format!("Bug: Recieved error from main thread.")
                    );
                }
            }
        };
        assert_eq!(next_buf.len(), 0);
        match hold_buf {
            None => { hold_buf = Some(next_buf) }
            Some(mut held) => {
                if let Some(delimiter_off) = find_bytes(&held, &delimiter) {
                    let leftover = delimiter_off + delimiter.len();
                    next_buf.extend_from_slice(&held[leftover..]);
                    held.truncate(leftover);
                    full_sender.send(Message::Line(held)).with_context(||
                        format!("Bug: Main thread hung up on reader (while sending again).")
                    )?;
                    hold_buf = Some(next_buf);
                    continue;
                }
                let mut offset: usize = held.len();
                loop {
                    #[allow(unused_mut)]
                    let mut next_size: usize = ((offset / PAGE) + 1) * PAGE;
                    held.resize(next_size, 0u8);
                    match si.read(&mut held[offset..next_size]) {
                        Ok(0) => {
                            // EOF
                            held.truncate(offset);
                            full_sender.send(Message::Line(held)).with_context(||
                                format!("Bug: Main thread hung up on reader (while sending last).")
                            )?;
                            full_sender.send(Message::End(Ok(()))).with_context(||
                                format!("Bug: Main thread hung up on reader (while sending EOF).")
                            )?;
                            hold_buf = None;
                            return Ok(());
                        }
                        Ok(bytes) => {
                            held.truncate(offset+bytes);
                            let search_start: usize = 
                                max(0isize, (offset as isize)-(delimiter.len() as isize)+1isize)
                                .try_into().expect("positive");
                            match find_bytes(&held[search_start..], &delimiter) {
                                Some(delimiter_off) => {
                                    let leftover = delimiter_off + delimiter.len();
                                    next_buf.extend_from_slice(&held[leftover..]);
                                    held.truncate(leftover);
                                    full_sender.send(Message::Line(held)).with_context(||
                                        format!("Bug: Main thread hung up on reader (while sending).")
                                    )?;
                                    hold_buf = Some(next_buf);
                                    break;
                                }
                                None => {
                                    offset += bytes;
                                }
                            }
                        }
                        Err(e) => {
                            return Err(e).with_context(||
                                format!("Error reading stdin")
                            );
                        }
                    };
                }
            }
        }
    }
}

fn writer(
    args: &'static Args,
    full_receiver: Sender<Message>,
    empty_sender: Receiver<Message>,
) -> Result<()> {
    let stdout_h = std::io::stdout();
    let so = stdout_h.lock();
    return Ok(());
}

fn amain(args: &'static Args) -> Result<()> {
    let (full_sender, full_receiver) = bounded::<Message>(2);
    let (empty_sender, empty_receiver) = bounded::<Message>(2);
    let buffer = ArrayQueue::<Vec<u8>>::new(args.buffer_lines);
    
    return Ok(());
//     }
//     let mut max_len = 1024;
//     let mut read_buf: Vec<u8> = Vec::new();
//     read_buf.extend(std::iter::repeat(0u8).take(max_len));
//     eprintln!("Hello, world!");
//     let mut in_files: VecDeque<InFileish> = VecDeque::new();
//     if args.files.len() > 0 {
//         for path_str in args.files {
//             let path = Path::new(&path_str);
//             let file = OpenOptions::new().read(true).open(path)
//                 .with_context(|| format!("Failed to open input file {}", &path_str.to_string_lossy()))
//                 ?;
//             in_files.push_back(InFileish::F(file));
//         }
//     } else {
//         in_files.push_back(InFileish::S(io::stdin()));
//     }
//     let delimiter: Vec<u8> = unescape_bytes(&args.delimiter.clone().into_vec())
//         .with_context(|| format!("Failed to parse delimiter"))
//     ?;
//     let mut out_file = io::stdout();
//     let mut in_file = match in_files.pop_front() {
//         Some(f) => f,
//         None => return Err(anyhow!("Logic Error: The length was checked right before this, so this should never happen.")),
//     };
//     let mut reading = true;
//     let mut writing = false;
//     let mut more_to_read = true;
//     loop {
//         if buffer.len() >= args.high_wm {
//             writing = true;
//         }
//         if buffer.len() == 0 {
//             writing = false;
//         }
//         if buffer.len() <= args.low_wm {
//             reading = true;
//         }
//         if buffer.len() >= buffer_lines {
//             reading = false;
//         }
//         let will_read = more_to_read && reading;
//         let will_write = writing;
//         
//     }
}

fn main() -> Result<()> {
    let mut args = Box::new(Args::parse());
    args.validate();
    let static_args: &'static Args = Box::leak(args);
    return amain(static_args);
}
