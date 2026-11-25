use crate::{DbError, Row, Value, types::ValueTag};

pub(crate) fn encode_row(row: &Row) -> Result<Vec<u8>, DbError> {
    let len = u32::try_from(row.len()).map_err(|_| DbError::CodecError("row too long".into()))?;
    let mut result = Vec::from(len.to_le_bytes());

    for value in row {
        result.push(value.tag() as u8);
        match value {
            Value::Integer(i) => result.extend_from_slice(&i.to_le_bytes()),
            Value::Float(f) => result.extend_from_slice(&f.to_le_bytes()),
            Value::Fsm(f) => result.extend_from_slice(&f.to_le_bytes()),
            Value::Bool(b) => result.push(*b as u8),
            Value::String(s) => {
                let str_bytes: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
                let str_len = u32::try_from(str_bytes.len())
                    .map_err(|_| DbError::CodecError("string too long".into()))?;
                result.extend_from_slice(&str_len.to_le_bytes());
                result.extend_from_slice(&str_bytes);
            }
            Value::Null => (),
        }
    }

    Ok(result)
}

fn read_array<'a, const N: usize>(
    iter: &mut impl Iterator<Item = &'a u8>,
) -> Result<[u8; N], DbError> {
    let mut buf = [0u8; N];
    for slot in &mut buf {
        *slot = *iter
            .next()
            .ok_or_else(|| DbError::CodecError("unexpected EOF".into()))?;
    }
    Ok(buf)
}

pub(crate) fn decode_row(bytes: &[u8]) -> Result<Row, DbError> {
    let mut iter = bytes.iter();
    let count = u32::from_le_bytes(read_array(&mut iter)?) as usize;
    let mut result = Row::new();

    for _ in 0..count {
        let tag_byte = *iter
            .next()
            .ok_or_else(|| DbError::CodecError("unexpected EOF".into()))?;
        let value = match ValueTag::try_from(tag_byte)? {
            ValueTag::Integer => Value::Integer(i64::from_le_bytes(read_array(&mut iter)?)),
            ValueTag::Float => Value::Float(f64::from_le_bytes(read_array(&mut iter)?)),
            ValueTag::Fsm => Value::Fsm(u16::from_le_bytes(read_array(&mut iter)?)),
            ValueTag::Bool => match *iter
                .next()
                .ok_or_else(|| DbError::CodecError("unexpected EOF".into()))?
            {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                _ => return Err(DbError::CodecError("invalid bool".into())),
            },
            ValueTag::Null => Value::Null,
            ValueTag::String => {
                let string_len = u32::from_le_bytes(read_array(&mut iter)?) as usize;
                let string_bytes: Vec<u8> = iter.by_ref().take(string_len).copied().collect();
                if string_bytes.len() != string_len {
                    return Err(DbError::CodecError("unexpected EOF in string".into()));
                }
                Value::String(
                    String::from_utf16le(&string_bytes)
                        .map_err(|_| DbError::CodecError("invalid utf16".into()))?,
                )
            }
        };
        result.push(value);
    }

    if iter.next().is_some() {
        return Err(DbError::CodecError("trailing bytes".into()));
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;

    #[test]
    fn roundtrip_various_rows() {
        let rows: Vec<Row> = vec![
            vec![],
            vec![Value::Null],
            vec![
                Value::Integer(0),
                Value::Integer(-42),
                Value::Integer(i64::MIN),
                Value::Integer(i64::MAX),
            ],
            vec![Value::Float(0.0), Value::Float(-3.5), Value::Float(123.456)],
            vec![Value::Fsm(0), Value::Fsm(7), Value::Fsm(u16::MAX)],
            vec![Value::Bool(false), Value::Bool(true)],
            vec![
                Value::String(String::new()),
                Value::String("hello".to_string()),
                Value::String("привет 🦀".to_string()),
            ],
            vec![
                Value::Integer(1),
                Value::String("alice".to_string()),
                Value::Float(99.5),
                Value::Fsm(2),
                Value::Bool(true),
                Value::Null,
            ],
        ];

        for row in rows {
            let encoded = encode_row(&row).expect("encode_row should succeed");
            let decoded = decode_row(&encoded).expect("decode_row should succeed");
            assert_eq!(decoded, row);
        }
    }

    #[test]
    fn decode_rejects_malformed_rows() {
        assert!(decode_row(&[]).is_err());
        assert!(decode_row(&[0xFF, 0xFF, 0xFF, 0xFF]).is_err());

        let sample = vec![
            Value::Integer(42),
            Value::String("hello".to_string()),
            Value::Bool(true),
            Value::Null,
        ];
        let encoded = encode_row(&sample).expect("encode_row should succeed");
        assert!(!encoded.is_empty());

        assert!(decode_row(&encoded[..1]).is_err());
        assert!(decode_row(&encoded[..encoded.len() - 1]).is_err());

        let mut with_trailing = encoded.clone();
        with_trailing.push(0x00);
        assert!(decode_row(&with_trailing).is_err());
    }
}
