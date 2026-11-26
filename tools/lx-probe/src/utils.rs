// Native LX utilities run inside the limited worker. Check sizes before allocating
// outside the QuickJS heap, especially for decompression and random bytes.
use std::io::Write;

use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use rquickjs::{Ctx, Function, Object, Result, TypedArray};

use super::{
    crypto::{self, AesMode},
    worker::js_error,
};

const LIMIT: usize = 4 * 1024 * 1024;

fn bytes<'a>(value: &'a TypedArray<'_, u8>) -> Result<&'a [u8]> {
    // No JS is invoked while any of these slices is borrowed.
    unsafe { value.as_bytes() }
        .filter(|b| b.len() <= LIMIT)
        .ok_or_else(|| js_error("invalid or oversized byte array"))
}

fn aes<'js>(
    ctx: Ctx<'js>,
    input: TypedArray<u8>,
    mode: String,
    key: TypedArray<u8>,
    iv: TypedArray<u8>,
) -> Result<TypedArray<'js, u8>> {
    let mode = AesMode::parse(&mode).map_err(js_error)?;
    let output =
        crypto::aes_encrypt(mode, bytes(&key)?, bytes(&iv)?, bytes(&input)?).map_err(js_error)?;
    TypedArray::new_copy(ctx, output)
}

fn rsa<'js>(
    ctx: Ctx<'js>,
    input: TypedArray<u8>,
    pem: TypedArray<u8>,
) -> Result<TypedArray<'js, u8>> {
    let output =
        crypto::rsa_public_encrypt_nopad_1024(bytes(&input)?, bytes(&pem)?).map_err(js_error)?;
    TypedArray::new_copy(ctx, output)
}

fn random<'js>(ctx: Ctx<'js>, size: usize) -> Result<TypedArray<'js, u8>> {
    if size > LIMIT {
        return Err(js_error("random byte limit exceeded"));
    }
    let output = crypto::random_bytes(size).map_err(js_error)?;
    TypedArray::new_copy(ctx, output)
}

fn zlib<'js>(ctx: Ctx<'js>, input: TypedArray<u8>, inflate: bool) -> Result<TypedArray<'js, u8>> {
    let input = bytes(&input)?;
    let error = || js_error("invalid zlib stream or output limit exceeded");
    let output = if inflate {
        let mut decoder = Decompress::new(true);
        let mut output = Vec::new();
        let mut block = [0; 8192];
        loop {
            let before = decoder.total_in() as usize;
            let status = decoder
                .decompress(&input[before..], &mut block, FlushDecompress::None)
                .map_err(|_| error())?;
            let count = decoder.total_out() as usize - output.len();
            if count > LIMIT - output.len() {
                return Err(error());
            }
            output.extend_from_slice(&block[..count]);
            if status == Status::StreamEnd {
                break;
            }
            if count == 0 && before == decoder.total_in() as usize {
                return Err(error());
            }
        }
        output
    } else {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input).map_err(|_| error())?;
        encoder.finish().map_err(|_| error())?
    };
    TypedArray::new_copy(ctx, output)
}

pub fn native(ctx: Ctx<'_>) -> Result<Object<'_>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("aes", Function::new(ctx.clone(), aes)?)?;
    obj.set("rsa", Function::new(ctx.clone(), rsa)?)?;
    obj.set("random", Function::new(ctx.clone(), random)?)?;
    obj.set("zlib", Function::new(ctx.clone(), zlib)?)?;
    obj.set(
        "md5",
        Function::new(ctx, |arr: TypedArray<u8>| -> Result<String> {
            Ok(crypto::md5_hex(bytes(&arr)?))
        })?,
    )?;
    Ok(obj)
}
