//! The sandbox launcher on its own, for the thumbnail and search tests. On a
//! server, the server's own program is the launcher. It also reads the text
//! of Office files and the covers and details of e-books, as the server's
//! program does, and reads Google Drive's files from Takeout archives.

use accordion_core::{takeout, texts, thumbnails};
use accordion_drive::TAKEOUT_PRODUCTS;

fn main() {
    let mut args = std::env::args_os().skip(1);
    match args.next() {
        Some(command) if command == texts::office::COMMAND => {
            std::process::exit(texts::office::main(args, texts::MAX_TEXT));
        }
        Some(command) if command == thumbnails::book::COMMAND => {
            std::process::exit(thumbnails::book::main(args));
        }
        Some(command) if command == takeout::COMMAND => {
            std::process::exit(takeout::reader::main(args, &TAKEOUT_PRODUCTS));
        }
        _ => accordion_sandbox::main(),
    }
}
