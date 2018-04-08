extern crate nix;

use std::io;
use std::io::prelude::*;
use std::io::Write;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::env::args;
use std::path::Path;
use std::cmp;

use nix::sys::termios;

/// A data type that represents where in the console window something resides.
/// Indexing starts at 0 (even though the VT100 escape sequences expect
/// coordinates starting at 1), because mixing 1-based indexing with 0-based
/// indexing lead to errors. Pos { col: 0, row: 0 } corresponds to the top left
/// corner of the terminal.
#[derive(Debug, Clone, Copy)]
struct Pos {
    col: usize,
    row: usize,
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

struct Cursor {
    /// The position of the cursor in the terminal window.
    pos: Pos,
    /// Since lines may take up several rows, the specific line with the cursor
    /// cannot simply be calculated with `pos`, so the index of the line in the
    /// lines list needs to be stored.
    line: usize,
    /// To the same reason as above, there is no way to retrieve the actual
    /// byte in line under cursor, so the absolute offset from the line's start
    /// needs to be stored here.
    byte: usize,
    /// In order to be able to go up and down along the ends of lines of
    /// different lengths (including 0), this flag needs to be set to determine
    /// whether to go to the same column in the next row or to its end.
    is_at_eol: bool,
}

struct Editor {
    // Note that this does not always report the actual position of the cursor.
    // Instead, it is the _desired_ position, i.e. what user sets. It may be
    // that for rendering purposes the cursor is temporarily relocated, but then
    // always set back to this position. This also means that when it's
    // temporarily relocated, this field shall not be updated.
    cursor: Cursor,
    window_width: usize,
    window_height: usize,
    // Used to coalesce writes into a single buffer to then flush it in one go
    // to avoid excessive IO overhead.
    write_buf: Vec<u8>,
    // Store each line as a separate string in a vector. Note that there is
    // a distinction between rows and lines. A line is the string of text until
    // the new-line character, as stored in the file, while a row is the
    // rendered string. This means a line may wrap several rows.
    lines: Vec<Vec<u8>>,
    // The zero-based index into `lines` of the first line to show.
    line_offset: usize,
    // The first character of the row in line that should be drawn. Always
    // a multiple of `window_width`. Also zero-based.
    line_offset_byte: usize,
}

fn init_log() {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open("/tmp/kilo-rust.log")
        .unwrap();
}

fn log(buf: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .append(true)
        .open("/tmp/kilo-rust.log")
        .unwrap();
    file.write("NEW LOG ENTRY\n".as_bytes()).unwrap();
    file.write(&buf).unwrap();
    file.write("\n".as_bytes()).unwrap();
    file.flush().unwrap();
}

impl Editor {
    pub fn new() -> Editor {
        Editor {
            cursor: Cursor { pos: Pos { row: 0, col: 0 }, line: 0, byte: 0, is_at_eol: false },
            window_width: 0,
            window_height: 0,
            write_buf: vec![],
            lines: vec![],
            line_offset: 0,
            line_offset_byte: 0,
        }
    }

    pub fn open_file(path: &Path) -> Editor {
        let mut editor = Editor::new();

        // TODO error handling: somehow let user know that we could not open file
        if let Ok(mut file) = File::open(path) {
            let mut buf = vec![];
            file.read_to_end(&mut buf).unwrap();

            // TODO might need to match \r\n as well
            let lines = buf.split(|b| *b == '\n' as u8);
            // Try to get an esimate of the number of lines in file.
            let size_hint = {
                let (lower, upper) = lines.size_hint();
                if let Some(upper) = upper { upper } else { lower }
            };

            if size_hint > 0 {
                editor.lines.reserve(size_hint);
            }

            editor.lines = lines
                .map(|line| line.to_vec())
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
            '\x1b' => self.handle_esc_seq_key(),
            _ => self.handle_input(c)
        }
    }

    fn handle_esc_seq_key(&mut self) {
        if let Some(key) = self.read_esc_seq_to_key() {
            match key {
                Key::ArrowUp => self.cursor_up(),
                Key::ArrowDown => self.cursor_down(),
                Key::ArrowLeft => self.cursor_left(),
                Key::ArrowRight => self.cursor_right(),
                Key::PageUp => {
                    let rows = cmp::min(self.window_height, self.cursor.pos.row);
                    for _ in 0..rows {
                        self.cursor_up();
                    }
                },
                Key::PageDown => {
                    let rows_left = self.lines.len() - self.cursor.pos.row;
                    let rows = cmp::min(self.window_height, rows_left);
                    for _ in 0..rows {
                        self.cursor_down();
                    }
                },
                Key::Home => {
                    // TODO adjust this to line wrapping
                    self.cursor.pos = Pos { row: 0, col: 0 };
                    self.line_offset = 0;
                }
                Key::End => {
                    // TODO adjust this to line wrapping
                    self.cursor.pos = Pos {  col: 0, row: self.window_height - 1 };
                    self.line_offset = self.lines.len() - self.window_height;
                }
                _ => (),
            }
        }
    }

    fn cursor_down(&mut self) {
        // Check if cursor is at the bottom of the window.
        // FIXME doesn't work
        if self.cursor.pos.row == self.window_height - 1 {
            self.scroll_down();
        }

        // Note that this is indexed from the beginning of the line, whereas
        // end_of_row is indexed from the beginning of the row.
        let row_last_byte = self.cursor.byte + self.end_of_row() - self.cursor.pos.col;
        let bytes_left_in_line = {
            let line_len = self.lines[self.cursor.line].len();
            if row_last_byte + 1 >= line_len {
                0
            } else {
                line_len - row_last_byte - 1
            }
        };

        if bytes_left_in_line > 0 {
            // We're not at the end of the line, which is merely wrapped, so
            // just go down one row staying on the same line.
            let next_row_len = cmp::min(bytes_left_in_line, self.window_width);
            let col = {
                if self.cursor.is_at_eol {
                    next_row_len - 1
                } else {
                    cmp::min(self.cursor.pos.col, next_row_len - 1)
                }
            };

            self.cursor.pos.row += 1;
            self.cursor.pos.col = col;
            self.cursor.byte = row_last_byte + 1 + col;
        } else if self.cursor.line + 1 < self.lines.len() {
            // We're at the end of the line so go down one row to the next
            // line if cursor is not already on the last line.
            self.cursor.line += 1;

            // Next line might be shorter than current cursor column position.
            let col = {
                let line = &self.lines[self.cursor.line];
                if line.is_empty() {
                    0
                } else if self.cursor.is_at_eol {
                    cmp::min(line.len(), self.window_width) - 1
                } else {
                    cmp::min(line.len() - 1, self.cursor.pos.col)
                }
            };

            self.cursor.pos.row += 1;
            self.cursor.pos.col = col;
            self.cursor.byte = col;
        }
    }

    fn scroll_down(&mut self) {
        // The top row may be part of a wrapped line, so need to check if we
        // need to advance to the next line or just adjust the byte offset
        // from which to show the line.
        if self.line_offset_byte + self.window_width < self.lines[self.line_offset].len() {
            self.line_offset_byte += self.window_width;
        } else if self.line_offset < self.lines.len() - 1 {
            self.line_offset += 1;
            self.line_offset_byte = 0;
        }
    }

    fn cursor_up(&mut self) {
        // Cursor may have reached the top of the window.
        if self.cursor.pos.row == 0 {
            self.scroll_up();
        }

        if self.cursor.byte >= self.window_width {
            // Line is wrapped so we don't have to skip to the previous line,
            // only the row.
            self.cursor.byte -= self.window_width;
            self.cursor.pos.row -= 1;
        } else if self.cursor.line > 0 {
            // Cursor is on the first row of this line, so go to the previous
            // line.
            self.cursor.line -= 1;
            self.cursor.pos.row -= 1;

            // Previous line might be shorter than current cursor column
            // position, in which case the cursor needs to be moved to its end,
            // or it might be wrapping, in which case the cursor needs to be
            // positioned on the last wrap of the line.
            let line = &self.lines[self.cursor.line];
            if line.is_empty() {
                self.cursor.pos.col = 0;
                self.cursor.byte = 0;
            } else {
                if line.len() <= self.window_width {
                    let col = {
                        if self.cursor.is_at_eol {
                            line.len() - 1
                        } else {
                            cmp::min(line.len() - 1, self.cursor.pos.col)
                        }
                    };

                    self.cursor.pos.col = col;
                    self.cursor.byte = col;
                } else {
                    // Use integer truncation to first get the number of full
                    // rows this line is broken up into.
                    let last_row_first_byte = (line.len() / self.window_width) * self.window_width;
                    let col = {
                        let last_row_len = line.len() - last_row_first_byte;
                        if self.cursor.is_at_eol {
                            last_row_len - 1
                        } else {
                            cmp::min(last_row_len - 1, self.cursor.pos.col)
                        }
                    };

                    self.cursor.byte = last_row_first_byte + col;
                    self.cursor.pos.col = col;
                }
            }
        }
    }

    fn scroll_up(&mut self) {
        // The top row may be part of a wrapped line, so need to check if we
        // need to advance to the previous line or just adjust the byte offset
        // from which to show the line.
        if self.line_offset_byte > self.window_width {
            self.line_offset -= self.window_width;
        } else if self.line_offset > 0 {
            self.line_offset -= 1;
            self.line_offset_byte = 0;
        }
    }

    fn cursor_left(&mut self) {
        if self.cursor.pos.col > 0 {
            if self.cursor.pos.col == self.end_of_row() {
                self.cursor.is_at_eol = false;
            }
            self.cursor.pos.col -= 1;
            self.cursor.byte -= 1;
        }
    }

    fn cursor_right(&mut self) {
        if self.cursor.byte + 1 < self.lines[self.cursor.line].len()
            && self.cursor.pos.col + 1 < self.window_width {
            self.cursor.pos.col += 1;
            self.cursor.byte += 1;
            if self.cursor.pos.col == self.end_of_row() {
                self.cursor.is_at_eol = true;
            }
        }
    }

    fn end_of_row(&self) -> usize {
        let line = &self.lines[self.cursor.line];
        if line.is_empty() {
            0
        } else {
            assert!(self.window_width > 0);
            cmp::min(line.len(), self.window_width) - 1
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

    fn handle_input(&mut self, c: char) {
    }

    fn refresh_screen(&mut self) {
        // Query window size as it may have been changed since the last redraw.
        // TODO if possible, listen to window resize events.
        self.update_window_size();
        // Hide cursor while redrawing to avoid glitching.
        self.hide_cursor();
        self.move_cursor(Pos { row: 0, col: 0 });
        // Append text to write buffer while clearing old data.
        self.build_rows();
        // (Rust giving me crap for directly passing self.cursor.pos.)
        let cursor = self.cursor.pos;
        // Move cursor back to its original position.
        self.move_cursor(cursor);
        self.show_cursor();
        self.defer_esc_seq("?25h");
        self.flush_write_buf();
    }

    fn build_rows(&mut self) {
        let mut n_rows_drawn = 0;

        for line in self.lines.iter().skip(self.line_offset) {
            if n_rows_drawn == self.window_height {
                break;
            }

            // The line might be longer than the width of our window, so it needs
            // to be split accross rows and wrapped. Count how many bytes are left in
            // the row to draw.
            let (mut n_bytes_left, mut offset) = {
                if n_rows_drawn == 0 {
                    // This is the first line to draw which may not be drawn
                    // from its first byte if window begins after a wrap.
                    (line.len() - self.line_offset_byte, self.line_offset_byte)
                } else {
                    (line.len(), 0)
                }
            };

            if n_bytes_left == 0 {
                // Clear row.
                self.write_buf.extend_from_slice("\x1b[K".as_bytes());
                // An empty line is just a line break.
                self.write_buf.extend_from_slice("\r\n".as_bytes());
                n_rows_drawn += 1;
            } else {
                // Split up line into rows.
                while n_bytes_left > 0 && n_rows_drawn < self.window_height {
                    // Clear row.
                    // TODO we should use self.clear_row function but can't due to ownership
                    self.write_buf.extend_from_slice("\x1b[K".as_bytes());

                    let end = offset + cmp::min(self.window_width, n_bytes_left);
                    let row = &line[offset..end];

                    offset += row.len();
                    n_bytes_left -= row.len();
                    n_rows_drawn += 1;

                    self.write_buf.extend_from_slice(row);
                    // Don't put a new line on the last row.
                    if n_rows_drawn < self.window_height {
                        self.write_buf.extend_from_slice("\r\n".as_bytes());
                    }
                }
            }
        }

        // There may not be enough text to fill all the rows of the window, so
        // fill the rest with '~'s.
        let n_rows_left = self.window_height - n_rows_drawn;
        if n_rows_left > 0 {
            for _ in 1..(n_rows_left - 1) {
                self.write_buf.extend_from_slice("~\r\n".as_bytes());
                self.clear_row();
            }

            // Don't put a new line on our last row as that will make the terminal
            // scroll down.
            self.write_buf.extend_from_slice("~".as_bytes());
            self.clear_row();
        }
    }

    fn flush_write_buf(&mut self) {
        io::stdout().write(&self.write_buf).unwrap();
        io::stdout().flush().unwrap();
        // Does not alter its capacity.
        self.write_buf.clear();
    }

    fn move_cursor(&mut self, pos: Pos) {
        self.defer_esc_seq(&format!("{};{}H", pos.row + 1, pos.col + 1));
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

    fn clear_row(&mut self) {
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
        let bottom_right_corner = self.cursor_pos();
        self.window_width = bottom_right_corner.col + 1;
        self.window_height = bottom_right_corner.row + 1;
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
        let row: usize = response[row_pos..semicolon_pos].parse().unwrap();

        // Skip the first integer.
        assert!(semicolon_pos < response.len());
        let response = &response[semicolon_pos..];

        let col_pos = response.find(char::is_numeric).unwrap();
        assert!(col_pos < response.len());
        let col: usize = response[col_pos..].parse().unwrap();

        Pos { col: col - 1, row: row - 1 }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        // Restore user's screen.
        self.clear_screen();
    }
}

fn main() {
    init_log();
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
