// src/path.rs

use std::path::{Path, PathBuf};

/// 将相对路径转换为基于当前工作目录的绝对路径
pub fn to_absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}