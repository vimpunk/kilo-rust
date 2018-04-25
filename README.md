# kilo-rust

## What
This is a small educational project whereby I (try to) implement a basic text editor, which begun by porting [antirez's kilo editor](https://github.com/antirez/kilo) to Rust but has since then taken its own direction (since I prefer to write my own code) with the introduction of line wrapping, which kilo (as of writing this) does not support.

## Why
I thought this might be a sufficiently interesting yet not too overwhelming first project while reading the amazing [Rust book](https://doc.rust-lang.org/stable/book/second-edition/) and learning Rust.

In the improbable case that someone should ever read the code, I humbly ask you, dear improbable person, that if you find anything that merits constructive critcism, please don't hesitate to share it--e.g. by opening an issue. It would be highly appreciated!

## Try it
It doesn't really work yet (it shows the contents of a file which you can _sort of_ navigate, though this is still buggy), but if you
want to see it not working, clone the repo and run `cargo run <filename>` from the project's root directory.

## Disclaimer
There are no plans to develop it beyond achieving basic functionality and familiarizing myself with Rust.
