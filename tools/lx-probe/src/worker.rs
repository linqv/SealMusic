use std::{
    cell::Cell,
    io::{self, BufRead, Read, Write},
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig, general_purpose::STANDARD},
};
use rquickjs::{CatchResultExt, Context, Ctx, Error, Function, Object, Runtime, TypedArray};
use serde_json::json;

use crate::FRAME_LIMIT;

const BASE64_DECODE: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

fn js_error(message: &str) -> Error {
    Error::new_from_js_message("lx", "host", message)
}

fn bytes_from<'js>(
    ctx: Ctx<'js>,
    text: String,
    encoding: String,
) -> rquickjs::Result<TypedArray<'js, u8>> {
    let bytes = match encoding.to_ascii_lowercase().as_str() {
        "utf8" | "utf-8" => text.into_bytes(),
        "base64" => {
            let compact: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
            BASE64_DECODE.decode(compact).map_err(|_| js_error("invalid base64"))?
        }
        "hex" => text
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map_while(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
            .collect(),
        _ => return Err(js_error("unsupported lx.utils.buffer.from encoding")),
    };
    TypedArray::new_copy(ctx, bytes)
}

fn bytes_to_string(array: TypedArray<u8>, encoding: String) -> rquickjs::Result<String> {
    // The view is used synchronously here. No JavaScript runs while it is borrowed.
    let bytes = unsafe { array.as_bytes() }.ok_or_else(|| js_error("detached Uint8Array"))?;
    match encoding.to_ascii_lowercase().as_str() {
        "utf8" | "utf-8" => Ok(String::from_utf8_lossy(bytes).into_owned()),
        "base64" => Ok(STANDARD.encode(bytes)),
        "hex" => Ok(bytes.iter().map(|b| format!("{b:02x}")).collect()),
        _ => Err(js_error("unsupported lx.utils.buffer.bufToString encoding")),
    }
}

fn emit(frame: String) -> rquickjs::Result<()> {
    if frame.len() > FRAME_LIMIT {
        return Err(js_error("oversize IPC frame"));
    }
    if frame.contains(['\n', '\r']) {
        return Err(js_error("invalid IPC frame"));
    }
    writeln!(io::stdout().lock(), "{frame}").map_err(|_| js_error("closed IPC channel"))
}

fn drive<'js>(ctx: Ctx<'js>, deadline: &Cell<Instant>) -> Result<()> {
    let install: Function = ctx.eval(include_str!("bridge.js"))?;
    let dispatch: Function = install.call((
        Function::new(ctx.clone(), emit)?,
        Function::new(ctx.clone(), bytes_from)?,
        Function::new(ctx.clone(), bytes_to_string)?,
    ))?;
    let mut input = io::stdin().lock();
    let mut frame = String::new();
    let mut phase = 0_u64;
    loop {
        frame.clear();
        let size = input.by_ref().take(FRAME_LIMIT as u64 + 2).read_line(&mut frame)?;
        if size == 0 {
            return Ok(());
        }
        ensure!(size <= FRAME_LIMIT + 1 && frame.ends_with('\n'), "invalid IPC frame");
        deadline.set(Instant::now() + Duration::from_secs(2));
        let msg: Object = ctx.json_parse(frame.as_bytes())?.get()?;
        let event: String = msg.get("event")?;
        if event != "callback" {
            phase = msg.get("id")?;
        }
        let result = (|| -> rquickjs::Result<()> {
            dispatch.call::<_, ()>((msg.clone(),))?;
            if event == "init" {
                ctx.eval::<(), _>(msg.get::<_, String>("script")?)?;
            }
            while ctx.execute_pending_job() {
                if Instant::now() >= deadline.get() {
                    return Err(js_error("script execution deadline exceeded"));
                }
            }
            Ok(())
        })()
        .catch(&ctx);
        if let Err(error) = result {
            emit(json!({"event":"error","id":phase,"error":error.to_string().chars().take(512).collect::<String>()}).to_string())?;
            return Ok(());
        }
    }
}

pub fn run() -> Result<()> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(48 * 1024 * 1024);
    runtime.set_max_stack_size(1024 * 1024);
    let deadline = Rc::new(Cell::new(Instant::now() + Duration::from_secs(2)));
    let limit = deadline.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= limit.get())));
    let context = Context::full(&runtime)?;
    context.with(|ctx| drive(ctx, &deadline))
}
