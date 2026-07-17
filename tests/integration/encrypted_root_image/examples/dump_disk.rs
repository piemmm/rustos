//! Developer helper: write the shared encrypted-root fixture image to
//! `/tmp/encrypted-root.img` so it can be inspected outside the test harness.
use tairix_test_encrypted_root_image as img;
fn main() {
    let mut bytes = img::build_image().expect("build image");
    let want =
        usize::try_from(img::TOTAL_SECTORS).expect("sector count fits usize") * img::SECTOR_BYTES;
    bytes.resize(want, 0);
    std::fs::write("/tmp/encrypted-root.img", &bytes).expect("write");
    println!("wrote /tmp/encrypted-root.img ({} bytes)", bytes.len());
}
