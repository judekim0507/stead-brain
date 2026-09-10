// SPDX-License-Identifier: MIT
// Vendored from pie-coding-agent 0.75.0; changes: moved into Stead's per-session harness.

pub const DEFAULT_MAX_LINES: usize = 2_000;
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Truncation {
    pub total_lines: usize,
    pub kept_lines: usize,
    pub truncated_lines: usize,
    pub total_bytes: usize,
    pub kept_bytes: usize,
}

impl Truncation {
    pub fn note(&self) -> Option<String> {
        (self.truncated_lines > 0).then(|| {
            format!(
                "[truncated: kept {}/{} lines, {} of {} bytes]",
                self.kept_lines, self.total_lines, self.kept_bytes, self.total_bytes
            )
        })
    }
}

pub fn truncate_head(text: &str, max_lines: usize, max_bytes: usize) -> (String, Truncation) {
    let total_bytes = text.len();
    let mut total_lines = 0;
    let mut kept_bytes = 0;
    let mut kept_lines = 0;
    let mut output = String::with_capacity(total_bytes.min(max_bytes));
    for line in text.split_inclusive('\n') {
        total_lines += 1;
        if kept_lines < max_lines && kept_bytes + line.len() <= max_bytes {
            output.push_str(line);
            kept_lines += 1;
            kept_bytes += line.len();
        }
    }
    let truncation = Truncation {
        total_lines,
        kept_lines,
        truncated_lines: total_lines - kept_lines,
        total_bytes,
        kept_bytes,
    };
    (output, truncation)
}

pub fn truncate_tail(text: &str, max_lines: usize, max_bytes: usize) -> (String, Truncation) {
    let total_bytes = text.len();
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let total_lines = lines.len();
    let mut kept_bytes = 0;
    let mut kept_lines = 0;
    let mut tail = Vec::new();
    for line in lines.iter().rev() {
        if kept_lines >= max_lines || kept_bytes + line.len() > max_bytes {
            break;
        }
        tail.push(*line);
        kept_lines += 1;
        kept_bytes += line.len();
    }
    tail.reverse();
    let truncation = Truncation {
        total_lines,
        kept_lines,
        truncated_lines: total_lines - kept_lines,
        total_bytes,
        kept_bytes,
    };
    (tail.concat(), truncation)
}
