use std::io::Write;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let bytes = std::fs::read(&a[1]).unwrap();
    let img = scrybox_server::cvscan::decode_rgb(&bytes).unwrap();
    let mut f = std::fs::File::create(&a[2]).unwrap();
    f.write_all(&img.px).unwrap();
    eprintln!("{}x{}", img.w, img.h);
}
