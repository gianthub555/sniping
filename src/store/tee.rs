//! An append-only CSV file and its store table at once: what the code writes goes to the file
//! (while `files_on`) and every complete line but the header goes to the table (`STORE_DB=on`), so
//! a writer only swaps its `OpenOptions::open` for `tee::append`.

use std::io::Write;

pub struct Tee {
    file: Option<std::fs::File>,
    table: &'static str,
    header: &'static str,
    db: bool,
    pending: Vec<u8>,
}

/// Opens `path` for appending (when files are on) teed to `table` (when the store takes rows).
/// None when neither would take anything (or the file cannot be opened and the store is off).
pub fn append(path: &str, table: &'static str, header: &'static str) -> Option<Tee> {
    let db = super::db_on();
    let file = if super::files_on() { std::fs::OpenOptions::new().create(true).append(true).open(path).ok() } else { None };
    (file.is_some() || db).then(|| Tee { file, table, header, db, pending: Vec::new() })
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(f) = self.file.as_mut() {
            f.write_all(buf)?;
        }
        if self.db {
            self.pending.extend_from_slice(buf);
            while let Some(i) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=i).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\n', '\r']);
                if !line.is_empty() && line != self.header {
                    super::mirror_line(self.table, self.header, line);
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.as_mut().map_or(Ok(()), |f| f.flush())
    }
}

/// The first line of `path` (the header check of a writer that sets aside a file with other
/// columns) without reading the whole file.
pub fn first_line(path: &str) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(f), &mut first).ok()?;
    Some(first.trim_end_matches(['\n', '\r']).to_string()).filter(|l| !l.is_empty())
}
