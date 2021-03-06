//! Utilities for dealing with annotated strings.

use protocol::{Meta, Remark, RemarkType};

/// A type for dealing with chunks of annotated text.
#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    /// Unmodified text chunk.
    Text {
        /// The text value of the chunk
        text: String,
    },
    /// Redacted text chunk with a note.
    Redaction {
        /// The redacted text value
        text: String,
        /// The rule that crated this redaction
        rule_id: String,
        /// Type type of remark for this redaction
        ty: RemarkType,
    },
}

impl Chunk {
    /// The text of this chunk.
    pub fn as_str(&self) -> &str {
        match *self {
            Chunk::Text { ref text } => &text,
            Chunk::Redaction { ref text, .. } => &text,
        }
    }

    /// Effective length of the text in this chunk.
    pub fn len(&self) -> usize {
        self.as_str().len()
    }
}

/// Chunks the given text based on remarks.
pub fn chunks_from_str(text: &str, meta: &Meta) -> Vec<Chunk> {
    let mut rv = vec![];
    let mut pos = 0;

    for remark in meta.remarks() {
        let (from, to) = match remark.range() {
            Some(range) => *range,
            None => continue,
        };

        if from > pos {
            if let Some(piece) = text.get(pos..from) {
                rv.push(Chunk::Text {
                    text: piece.to_string(),
                });
            } else {
                break;
            }
        }
        if let Some(piece) = text.get(from..to) {
            rv.push(Chunk::Redaction {
                text: piece.to_string(),
                rule_id: remark.rule_id().into(),
                ty: remark.ty(),
            });
        } else {
            break;
        }
        pos = to;
    }

    if pos < text.len() {
        if let Some(piece) = text.get(pos..) {
            rv.push(Chunk::Text {
                text: piece.to_string(),
            });
        }
    }

    rv
}

/// Concatenates chunks into a string and places remarks inside the given meta.
pub fn chunks_to_string(chunks: Vec<Chunk>, mut meta: Meta) -> (String, Meta) {
    let mut rv = String::new();
    let mut remarks = vec![];
    let mut pos = 0;

    for chunk in chunks {
        let new_pos = pos + chunk.len();
        rv.push_str(chunk.as_str());
        if let Chunk::Redaction {
            ref rule_id, ty, ..
        } = chunk
        {
            remarks.push(Remark::with_range(ty, rule_id.clone(), (pos, new_pos)));
        }
        pos = new_pos;
    }

    *meta.remarks_mut() = remarks;
    (rv, meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunking() {
        let chunks = chunks_from_str(
            "Hello Peter, my email address is ****@*****.com. See you",
            &Meta {
                remarks: vec![Remark::with_range(
                    RemarkType::Masked,
                    "@email:strip",
                    (33, 47),
                )],
                ..Default::default()
            },
        );

        assert_eq_dbg!(
            chunks,
            vec![
                Chunk::Text {
                    text: "Hello Peter, my email address is ".into(),
                },
                Chunk::Redaction {
                    ty: RemarkType::Masked,
                    text: "****@*****.com".into(),
                    rule_id: "@email:strip".into(),
                },
                Chunk::Text {
                    text: ". See you".into(),
                },
            ]
        );

        assert_eq_dbg!(
            chunks_to_string(chunks, Default::default()),
            (
                "Hello Peter, my email address is ****@*****.com. See you".into(),
                Meta {
                    remarks: vec![Remark::with_range(
                        RemarkType::Masked,
                        "@email:strip",
                        (33, 47),
                    )],
                    ..Default::default()
                }
            )
        );
    }

}
