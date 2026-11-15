//! Minimal gzip writer for test corpora: any level (0 gives stored
//! blocks), and optionally many members, like pigz or BGZF output.

use std::io;

use libz_rs_sys as z;

/// Compresses `data` as one gzip member.
pub fn gzip_member(data: &[u8], level: i32) -> io::Result<Vec<u8>> {
    let mut strm = Box::new(z::z_stream::default());
    let ret = unsafe {
        z::deflateInit2_(
            &mut *strm,
            level,
            z::Z_DEFLATED,
            31,
            8,
            z::Z_DEFAULT_STRATEGY,
            z::zlibVersion(),
            std::mem::size_of::<z::z_stream>() as i32,
        )
    };
    if ret != z::Z_OK {
        return Err(io::Error::other(format!("deflateInit2 failed ({ret})")));
    }
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    let mut input = data;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let take = input.len().min(1 << 30);
        strm.next_in = input.as_ptr();
        strm.avail_in = take as z::uInt;
        strm.next_out = buf.as_mut_ptr();
        strm.avail_out = buf.len() as z::uInt;
        let flush = if take == input.len() {
            z::Z_FINISH
        } else {
            z::Z_NO_FLUSH
        };
        let ret = unsafe { z::deflate(&mut *strm, flush) };
        input = &input[take - strm.avail_in as usize..];
        out.extend_from_slice(&buf[..buf.len() - strm.avail_out as usize]);
        match ret {
            z::Z_STREAM_END => break,
            z::Z_OK | z::Z_BUF_ERROR => {}
            _ => {
                unsafe { z::deflateEnd(&mut *strm) };
                return Err(io::Error::other(format!("deflate failed ({ret})")));
            }
        }
    }
    unsafe { z::deflateEnd(&mut *strm) };
    Ok(out)
}

/// Compresses `data` as gzip, starting a new member every `member_size`
/// bytes of input (`None`: a single member).
pub fn gzip(data: &[u8], level: i32, member_size: Option<usize>) -> io::Result<Vec<u8>> {
    match member_size {
        None => gzip_member(data, level),
        Some(n) => {
            let mut out = Vec::new();
            for part in data.chunks(n.max(1)) {
                out.extend(gzip_member(part, level)?);
            }
            if data.is_empty() {
                out.extend(gzip_member(data, level)?);
            }
            Ok(out)
        }
    }
}
