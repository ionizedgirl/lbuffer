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
use std::pin::Pin;

use anyhow::anyhow;
use anyhow::Context as ErrContext;
use anyhow::Result;

use async_std::io;
use async_std::task;
use async_std::io::Error;
use async_std::io::Stdin;
use async_std::io::Stdout;
use async_std::io::Read;
use async_std::io::Write;
use async_std::fs::File;
use async_std::fs::OpenOptions;
use async_std::task::Context;
use async_std::task::Poll;
use async_std::path::Path;
use async_std::prelude::*;

use clap::Parser;
use clap::CommandFactory;

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
    fn poll_read(
        self: Pin<&mut Self>, 
        cx: &mut Context<'_>, 
        buf: &mut [u8]
    ) -> Poll<Result<usize, Error>> {
        match Pin::into_inner(self) {
            Self::F(f) => Read::poll_read(Pin::new(f), cx, buf),
            Self::S(f) => Read::poll_read(Pin::new(f), cx, buf),
        }
    }
}

async fn amain(args: Args) -> Result<()> {
    let mut stdout = io::stdout();
    let mut stdin = io::stdin();
    let buffer_lines = args.buffer_lines;
    let mut buffer: VecDeque<Vec<u8>> = VecDeque::with_capacity(buffer_lines);
    let mut max_len = 1024;
    let mut read_buf: Vec<u8> = Vec::new();
    read_buf.extend(std::iter::repeat(0u8).take(max_len));
    eprintln!("Hello, world!");
    let mut in_files: VecDeque<InFileish> = VecDeque::new();
    if args.files.len() > 0 {
        for path_str in args.files {
            let path = Path::new(&path_str);
            let file = OpenOptions::new().read(true).open(path).await
                .with_context(|| format!("Failed to open input file {}", &path_str.to_string_lossy()))
                ?;
            in_files.push_back(InFileish::F(file));
        }
    } else {
        in_files.push_back(InFileish::S(io::stdin()));
    }
    let delimiter: Vec<u8> = unescape_bytes(&args.delimiter.clone().into_vec())
        .with_context(|| format!("Failed to parse delimiter"))
    ?;
    let mut out_file = io::stdout();
    let mut in_file = match in_files.pop_front() {
        Some(f) => f,
        None => return Err(anyhow!("Logic Error: The length was checked right before this, so this should never happen.")),
    };
    let mut reading = true;
    let mut writing = false;
    let mut more_to_read = true;
    loop {
        if buffer.len() >= args.high_wm {
            writing = true;
        }
        if buffer.len() == 0 {
            writing = false;
        }
        if buffer.len() <= args.low_wm {
            reading = true;
        }
        if buffer.len() >= buffer_lines {
            reading = false;
        }
        let will_read = more_to_read && reading;
        let will_write = writing;
        
    }
}

fn main() -> Result<()> {
    let mut args = Args::parse();
    args.validate();
    task::block_on(async {
        amain(args).await
    })
}
