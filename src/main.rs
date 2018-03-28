extern crate nix;

use std::io;
use std::io::prelude::*;
use std::io::Write;
use std::io::BufReader;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::env::args;
use std::path::Path;
use std::cmp::min;

use nix::sys::termios;

/// A data type that represents where in the console window something resides.
/// Indexing starts at 1 (since this is what the VT100 escape sequences expect
/// as well) which corresponds to the top left corner of the terminal.
#[derive(Debug, Clone, Copy)]
struct Pos {
    col: i32,
    row: i32,
}

enum Key {
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
}

fn ctrl_mask(c: u8) -> u8 {
    c & 0x1f
}

struct Editor {
    // Note that this does not always report the actual position of the cursor.
    // Instead, it is the _desired_ position, i.e. what user sets. It may be
    // that for rendering purposes the cursor is temporarily relocated, but then
    // always set back to this position. This also means that when it's
    // temporarily relocated, this field shall not be updated.
    cursor: Pos,
    // The position of the bottom right corner of the window. This is used as
    // window size.
    bottom_right_corner: Pos,
    // Used to coalesce writes into a single buffer to then flush it in one go
    // to avoid excessive IO overhead.
    write_buf: Vec<u8>,
    // Store each row as a separate string in a vector.
    rows: Vec<String>,
    // The zero based index into `rows` of the first row to show.
    curr_row: i32,
}

impl Editor {
    pub fn new() -> Editor {
        Editor {
            cursor: Pos { row: 1, col: 1 },
            bottom_right_corner: Pos { row: 1, col: 1 },
            write_buf: vec![],
            rows: vec![],
            curr_row: 0,
        }
    }

    pub fn open_file(path: &Path) -> Editor {
        let mut editor = Editor::new();

        if let Ok(file) = File::open(path) {
            let reader = BufReader::new(file);
            let lines = reader.lines();
            // Try to get an esimate of the number of lines in file.
            let size_hint = {
                let (lower, upper) = lines.size_hint();
                if let Some(upper) = upper { upper } else { lower }
            };

            editor.rows.reserve(size_hint);
            // TODO correct error handling and may need to truncate \n
            editor.rows = lines
                .map(|line| line.expect("could not parse line!"))
                .collect();
        }

        editor
    }

    pub fn run(&mut self) {
        let mut buf: [u8; 1] = [0; 1];
        loop {
            self.refresh_screen();
            if let Ok(_) = io::stdin().read_exact(&mut buf) {
                let b = buf[0];
                if b == ctrl_mask('c' as u8) {
                    break;
                } else {
                    self.handle_key(b as char)
                }
            } else {
                break;
            }
        }
    }

    fn handle_key(&mut self, c: char) {
        match c {
            '\x1b' => {
                if let Some(key) = self.read_esc_seq_to_key() {
                    match key {
                        Key::ArrowUp if self.cursor.row > 1 => self.cursor.row -= 1,
                        Key::ArrowDown if self.cursor.row < self.bottom_right_corner.row => {
                            self.cursor.row += 1
                        }
                        Key::ArrowLeft if self.cursor.col > 1 => self.cursor.col -= 1,
                        Key::ArrowRight if self.cursor.col < self.bottom_right_corner.col => {
                            self.cursor.col += 1
                        }
                        // TODO for now these are equivalent but that'll change
                        Key::PageUp | Key::Home => self.cursor.row = 1,
                        Key::PageDown | Key::End => self.cursor.row = self.bottom_right_corner.row,
                        _ => (),
                    }
                }
            }
            _ => {
                println!("unhandled input: {}/{}", c as u8, c);
            }
        }
    }

    /// This function is called after encountering a \x1b escape character from
    /// stdin. It reads in the rest of the escape sequence and translates it to
    /// an optional Key value, or None, if no valid (or implemented) sequence
    /// was deteced.
    fn read_esc_seq_to_key(&mut self) -> Option<Key> {
        let mut buf: [u8; 3] = [0; 3];
        if let Ok(_) = io::stdin().read_exact(&mut buf[..2]) {
            if buf[0] as char == '[' {
                if buf[1] as char >= '0' && buf[1] as char <= '9' {
                    if let Err(_) = io::stdin().read_exact(&mut buf[2..3]) {
                        return None;
                    }
                    if buf[2] as char == '~' {
                        return match buf[1] as char {
                            '1' | '7' => Some(Key::Home),
                            '4' | '8' => Some(Key::End),
                            '3' => Some(Key::Delete),
                            '5' => Some(Key::PageUp),
                            '6' => Some(Key::PageDown),
                            _ => None,
                        };
                    }
                } else {
                    return match buf[1] as char {
                        'A' => Some(Key::ArrowUp),
                        'B' => Some(Key::ArrowDown),
                        'C' => Some(Key::ArrowRight),
                        'D' => Some(Key::ArrowLeft),
                        'H' => Some(Key::Home),
                        'F' => Some(Key::End),
                        _ => None,
                    };
                }
            } else if buf[0] as char == 'O' {
                return match buf[1] as char {
                    'H' => Some(Key::Home),
                    'F' => Some(Key::End),
                    _ => None,
                };
            }
        }
        None
    }

    fn refresh_screen(&mut self) {
        // Query window size as it may have been changed since the last redraw.
        // TODO if possible, listen to window resize events.
        self.update_window_size();
        // Hide cursor while redrawing to avoid glitching.
        self.hide_cursor();
        self.move_cursor(Pos { row: 1, col: 1 }); // Is this needed?
                                                  // Append text to write buffer while clearing old data.
        self.prepare_rows();
        // (Rust giving me crap for directly passing self.cursor.)
        let cursor = self.cursor;
        // Move cursor back to its original position.
        self.move_cursor(cursor);
        self.show_cursor();
        self.defer_esc_seq("?25h");
        self.flush_write_buf();
    }

    fn prepare_rows(&mut self) {
        let n_rows = self.bottom_right_corner.row;
        let n_cols = self.bottom_right_corner.col;

        let mut n_rows_drawn = 0;
        for (i, row) in self.rows.iter().enumerate() {
            if i as i32 == n_rows {
                break;
            }

            // Clear line.
            self.write_buf.extend_from_slice("\x1b[K".as_bytes());

            // The line might be longer than the width of our window, so it needs
            // to be split accross rows and wrapped.
            let mut n_bytes_left = row.len() as i32;
            let mut offset = 0;
            while n_bytes_left > 0 {
                let end = offset + min(n_cols, n_bytes_left) as usize;
                let row = &row.as_bytes()[offset..end];

                offset += row.len();
                n_bytes_left -= row.len() as i32;
                n_rows_drawn += 1;

                self.write_buf.extend_from_slice(row);
                // Don't put a new line on the last row.
                if n_rows_drawn < n_rows - 1 {
                    self.write_buf.extend_from_slice("\r\n".as_bytes());
                }
            }
        }

        // There may not be enough text to fill all the rows of the window, so
        // fill the rest with '~'s.
        let n_rows_left = n_rows - n_rows_drawn;
        if n_rows_left > 0 {
            for _ in 1..(n_rows_left - 1) {
                self.write_buf.extend_from_slice("~\r\n".as_bytes());
                self.clear_line();
            }

            // Don't put a new line on our last row as that will make the terminal
            // scroll down.
            self.write_buf.extend_from_slice("~".as_bytes());
            self.clear_line();
        }
    }

    fn flush_write_buf(&mut self) {
        io::stdout().write(&self.write_buf).unwrap();
        io::stdout().flush().unwrap();
        // Does not alter its capacity.
        self.write_buf.clear();
    }

    fn move_cursor(&mut self, pos: Pos) {
        self.defer_esc_seq(&format!("{};{}H", pos.row, pos.col));
    }

    fn hide_cursor(&mut self) {
        self.defer_esc_seq("?25l");
    }

    fn show_cursor(&mut self) {
        self.defer_esc_seq("?25h");
    }

    fn clear_screen(&mut self) {
        self.defer_esc_seq("2J");
    }

    fn clear_line(&mut self) {
        self.defer_esc_seq("K");
    }

    /// Appends the specified escape sequence to the write buffer which needs to
    /// be manually flushed for the sequence to take effect.
    fn defer_esc_seq(&mut self, cmd: &str) {
        self.write_buf.extend_from_slice(&format!("\x1b[{}", cmd).as_bytes());
    }

    /// Immeadiately sends the specified escape sequence to the terminal.
    fn send_esc_seq(&mut self, cmd: &str) {
        println!("\x1b[{}", cmd);
    }

    fn update_window_size(&mut self) {
        // Move cursor as far right and down as we can (set_cursor_pos not used
        // on purpose as it uses a different escape sequence which does not
        // ensure that it won't move the cursor beyond the confines of the
        // window while this does).
        self.send_esc_seq("999C");
        self.send_esc_seq("999B");
        self.bottom_right_corner = self.cursor_pos();
    }

    fn cursor_pos(&mut self) -> Pos {
        // Query cursor position.
        self.send_esc_seq("6n");

        // Read response from stdin. The response should look like this:
        // \x1b[<number>;<number>
        // So if we generously assume each number to be 3 digits long, 10
        // bytes should be enough to allocate only once.
        let mut response = String::with_capacity(10);
        for r in io::stdin().bytes() {
            match r {
                Ok(c) => {
                    if c == 'R' as u8 {
                        break;
                    } else {
                        response.push(c as char);
                    }
                }
                Err(_) => (),
            }
        }

        // Sometimes we receive a [6~ (which as far as I can tell is not a
        // valid escape sequence), so skip to the first \x1b character.
        let esc_pos = response.find('\x1b').unwrap();
        let response = &response[esc_pos + 1..];
        let row_pos = response.find(char::is_numeric).unwrap();
        let semicolon_pos = response.find(';').unwrap();
        assert!(row_pos < semicolon_pos);
        let row: i32 = response[row_pos..semicolon_pos].parse().unwrap();

        // Skip the first integer.
        assert!(semicolon_pos < response.len());
        let response = &response[semicolon_pos..];

        let col_pos = response.find(char::is_numeric).unwrap();
        assert!(col_pos < response.len());
        let col: i32 = response[col_pos..].parse().unwrap();

        Pos { col, row }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        // Restore user's screen.
        self.clear_screen();
    }
}

fn main() {
    // Save the terminal config as it was before entering raw mode with the
    // instantiation of the editor so that we can restore it on drop.
    let orig_termios = termios::tcgetattr(io::stdin().as_raw_fd()).unwrap();
    let mut raw_termios = orig_termios.clone();

    termios::cfmakeraw(&mut raw_termios);
    termios::tcsetattr(
        io::stdin().as_raw_fd(),
        termios::SetArg::TCSANOW,
        &raw_termios,
    ).unwrap();

    let args: Vec<String> = args().collect();
    if args.len() > 1 {
        Editor::open_file(Path::new(&args[1])).run();
    } else {
        Editor::new().run();
    }

    // Restore the original termios config.
    termios::tcsetattr(
        io::stdin().as_raw_fd(),
        termios::SetArg::TCSANOW,
        &orig_termios,
    ).unwrap();
}
