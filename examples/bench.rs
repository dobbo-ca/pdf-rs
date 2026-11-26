// Licensed under the Apache License, Version 2.0 (see LICENSE.txt). A derivative work of
// pdf-go's pkg/pdfcpu (see NOTICE): the classes below mirror tests/parity/go/oracle/bench.go.

//! `bench <dir> <n> [threads]`: for every file of `dir` whose name starts with a class and `__`
//! (`read-small__test.pdf`), runs the operation of the class `n` times and prints
//! `name TAB bytes TAB ns per run`, as `oracle -bench` does for Go. `threads` sets
//! `Limits::max_threads` (the default when omitted). A file whose operation fails is skipped, with
//! a line on stderr. The setup of an operation (the read that `decode-flate` and `image-*` start
//! from) is not timed. See `tests/parity/bench.sh` and docs/benchmarks.md.

use pdf::model::{EntryObject, XRefTable};
use pdf::object::{Object, StreamDict};
use pdf::{Cmd, Config, Error, WriteEnv};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use std::{env, fs};

/// The bytes an operation counts and the time of its timed part.
type Timed = Result<(u64, Duration), Error>;
type Op = fn(&[u8], &Config, &AtomicBool) -> Timed;

/// At most this many images per document, as `oracle -bench` (maxImages).
const MAX_IMAGES: usize = 64;

fn op_for(class: &str) -> Option<Op> {
    let op: Op = match class {
        "read-small" | "read-objstm" | "read-large" => read_op,
        "validate" => validate_op,
        "rvo" => rvo_op,
        "write-fallback" => write_op,
        "decode-flate" => decode_op,
        "encode-flate" => encode_op,
        "image-png" | "image-tiff" | "image-dct-gray" | "image-dct-rgb" | "image-dct-cmyk"
        | "image-ccitt" => images_op,
        _ => return None,
    };
    Some(op)
}

fn read_op(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Timed {
    let t = Instant::now();
    // The document is dropped inside the timed part.
    let r = pdf::api::read(data, cfg, cancel).map(drop);
    let d = t.elapsed();
    r.map(|()| (data.len() as u64, d))
}

fn validate_op(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Timed {
    let t = Instant::now();
    let r = pdf::api::validate(data, cfg, cancel);
    let d = t.elapsed();
    r.map(|()| (data.len() as u64, d))
}

fn rvo_op(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Timed {
    let t = Instant::now();
    let r = pdf::api::read_validate_and_optimize(data, cfg, cancel).map(drop);
    let d = t.elapsed();
    r.map(|()| (data.len() as u64, d))
}

fn write_op(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Timed {
    let env = WriteEnv::default();
    let t = Instant::now();
    let r = pdf::api::read(data, cfg, cancel)
        .and_then(|mut ctx| pdf::api::write_context(&mut ctx, &env, cancel))
        .map(drop);
    let d = t.elapsed();
    r.map(|()| (data.len() as u64, d))
}

/// The object numbers of the streams (the images only when `images`), ascending.
fn numbers(x: &XRefTable, images: bool) -> Vec<i64> {
    x.table
        .iter()
        .filter(|(_, e)| match &e.object {
            Some(EntryObject::Obj(Object::Stream(sd))) => {
                !images || sd.dict.subtype() == Some(b"Image".as_slice())
            }
            _ => false,
        })
        .map(|(n, _)| *n)
        .collect()
}

/// A copy of the stream of object `n` that has not been decoded.
fn fresh(x: &XRefTable, n: i64) -> Option<StreamDict> {
    match x.table.get(&n)?.object.as_ref()? {
        EntryObject::Obj(Object::Stream(sd)) => {
            let mut s = (**sd).clone();
            s.content = None;
            Some(s)
        }
        _ => None,
    }
}

fn decode_op(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Timed {
    let ctx = pdf::api::read(data, cfg, cancel)?;
    let mem = ctx.xref.mem().clone();
    let limit = i64::try_from(cfg.limits.decode_limit()).unwrap_or(i64::MAX);
    let nums = numbers(&ctx.xref, false);
    let mut total = 0u64;
    let t = Instant::now();
    for n in nums {
        let Some(mut sd) = fresh(&ctx.xref, n) else {
            continue;
        };
        if sd.decode_with_limit(limit, &mem, cancel).is_ok() {
            total += sd.content.as_ref().map_or(0, |c| c.len() as u64);
        }
    }
    Ok((total, t.elapsed()))
}

fn encode_op(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Timed {
    let x = XRefTable::new(Config {
        limits: cfg.limits,
        cmd: Cmd::Validate,
    });
    let mem = x.mem().clone();
    let buf = mem.bytes_from(data, data.len())?;
    let t = Instant::now();
    let mut sd = x.new_stream_dict_for_buf(buf);
    sd.encode(&mem, cancel)?;
    let d = t.elapsed();
    Ok((sd.raw.as_ref().map_or(0, |r| r.len() as u64), d))
}

fn images_op(data: &[u8], cfg: &Config, cancel: &AtomicBool) -> Timed {
    let mut ctx = pdf::api::read(data, cfg, cancel)?;
    let nums = numbers(&ctx.xref, true);
    let mut total = 0u64;
    let t = Instant::now();
    for n in nums.into_iter().take(MAX_IMAGES) {
        let Some(mut sd) = fresh(&ctx.xref, n) else {
            continue;
        };
        if let Ok(Some(img)) = pdf::api::extract_image(&mut ctx, &mut sd, b"Im0", n, cancel) {
            total += img.data.len() as u64;
        }
    }
    Ok((total, t.elapsed()))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let (dir, n, threads) = match args.as_slice() {
        [_, dir, n] => (dir, n, None),
        [_, dir, n, t] => (dir, n, t.parse::<usize>().ok()),
        _ => return Err("usage: bench <dir> <n> [threads]".to_string()),
    };
    let n: u32 = n.parse().unwrap_or(1).max(1);
    let mut cfg = Config::default();
    if let Some(t) = threads {
        cfg.limits.max_threads = t;
    }
    let cancel = AtomicBool::new(false);
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        let class = name.split("__").next().unwrap_or("");
        let Some(op) = op_for(class) else {
            continue;
        };
        let data = fs::read(format!("{dir}/{name}")).map_err(|e| e.to_string())?;
        let mut size = match op(&data, &cfg, &cancel) {
            Ok((size, _)) => size,
            Err(e) => {
                eprintln!("bench: skip {name}: {e}");
                continue;
            }
        };
        let mut total = Duration::ZERO;
        for _ in 0..n {
            let (s, d) = op(&data, &cfg, &cancel).map_err(|e| format!("{name}: {e}"))?;
            size = s;
            total += d;
        }
        println!("{name}\t{size}\t{}", total.as_nanos() / u128::from(n));
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("bench: {e}");
        std::process::exit(1);
    }
}
