// Debug tool: dump vips-twin resize raw bytes for parity vs the container's
// sharp raw output. usage: dumpres <img> <outW> <outH> <out.raw> [width-only]
use std::io::Write;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let bytes = std::fs::read(&a[1]).unwrap();
    let ow: usize = a[2].parse().unwrap();
    let oh: usize = a[3].parse().unwrap();
    let img = scrybox_server::cvscan::decode_rgb(&bytes).unwrap();
    let r = scrybox_server::cvscan::resize_fill(&img, ow, oh);
    let mut f = std::fs::File::create(&a[4]).unwrap();
    f.write_all(&r.px).unwrap();
    eprintln!("{}x{} -> {}x{} ({} B)", img.w, img.h, r.w, r.h, r.px.len());
}
