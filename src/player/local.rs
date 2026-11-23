use std::{fs::File, io::BufReader, path::Path};

use rodio::Decoder;

pub fn decode(path: &Path) -> Result<Decoder<BufReader<File>>, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Decoder::try_from(file).map_err(|e| format!("{}: {e}", path.display()))
}
