pub mod decoder;
pub mod frames;
pub mod webm;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
