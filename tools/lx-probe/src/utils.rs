// Native LX utilities run inside the limited worker. Check sizes before allocating
// outside the QuickJS heap, especially for decompression and random bytes.
use std::io::Write;

use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use openssl::{
    hash::{MessageDigest, hash},
    rand::rand_bytes,
    rsa::{Padding, Rsa},
    symm::{Cipher, encrypt},
};
use rquickjs::{Ctx, Function, Object, Result, TypedArray};

use super::worker::js_error;

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
    let cipher = match mode.to_ascii_lowercase().as_str() {
        "aes-128-cbc" => Cipher::aes_128_cbc(),
        "aes-192-cbc" => Cipher::aes_192_cbc(),
        "aes-256-cbc" => Cipher::aes_256_cbc(),
        "aes-128-ecb" => Cipher::aes_128_ecb(),
        "aes-192-ecb" => Cipher::aes_192_ecb(),
        "aes-256-ecb" => Cipher::aes_256_ecb(),
        _ => return Err(js_error("unsupported AES mode")),
    };
    let key = bytes(&key)?;
    let iv = bytes(&iv)?;
    if key.len() != cipher.key_len() || iv.len() != cipher.iv_len().unwrap_or(0) {
        return Err(js_error("invalid AES key or IV length"));
    }
    let output = encrypt(cipher, key, cipher.iv_len().map(|_| iv), bytes(&input)?)
        .map_err(|_| js_error("AES encryption failed"))?;
    TypedArray::new_copy(ctx, output)
}

fn rsa<'js>(
    ctx: Ctx<'js>,
    input: TypedArray<u8>,
    pem: TypedArray<u8>,
) -> Result<TypedArray<'js, u8>> {
    let pem = bytes(&pem)?;
    let key = Rsa::public_key_from_pem(pem)
        .or_else(|_| Rsa::public_key_from_pem_pkcs1(pem))
        .map_err(|_| js_error("invalid RSA public key"))?;
    let input = bytes(&input)?;
    let mut block = [0; 128];
    if input.len() > block.len() || key.size() != 128 {
        return Err(js_error("invalid RSA block or key size"));
    }
    block[128 - input.len()..].copy_from_slice(input);
    let mut output = vec![0; 128];
    key.public_encrypt(&block, &mut output, Padding::NONE)
        .map_err(|_| js_error("RSA encryption failed"))?;
    TypedArray::new_copy(ctx, output)
}

fn random<'js>(ctx: Ctx<'js>, size: usize) -> Result<TypedArray<'js, u8>> {
    if size > LIMIT {
        return Err(js_error("random byte limit exceeded"));
    }
    let mut output = vec![0; size];
    rand_bytes(&mut output).map_err(|_| js_error("random byte generation failed"))?;
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
            let digest =
                hash(MessageDigest::md5(), bytes(&arr)?).map_err(|_| js_error("MD5 failed"))?;
            Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
        })?,
    )?;
    Ok(obj)
}
