//! A DNS question for one IPv4 address and the reading of its answer. Pure:
//! the caller sends and receives the bytes.

const HEADER: usize = 12;
const TAIL: [u8; 5] = [0, 0, 1, 0, 1]; // end of name, type A, class IN

/// Writes the question into `packet` and returns its length.
pub fn query(packet: &mut [u8], host: &str, id: u16) -> Option<usize> {
    let mut n = HEADER;
    packet.get_mut(..n)?.fill(0);
    packet[..2].copy_from_slice(&id.to_be_bytes());
    packet[2] = 1; // recursion wanted
    packet[5] = 1; // one question
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        *packet.get_mut(n)? = label.len() as u8;
        packet.get_mut(n + 1..n + 1 + label.len())?.copy_from_slice(label.as_bytes());
        n += 1 + label.len();
    }
    packet.get_mut(n..n + TAIL.len())?.copy_from_slice(&TAIL);
    Some(n + TAIL.len())
}

/// The first IPv4 address in the answer to question `id`.
pub fn answer(packet: &[u8], id: u16) -> Option<[u8; 4]> {
    let is_reply = packet.get(2)? & 0x80 != 0;
    let failed = packet.get(3)? & 15 != 0;
    if packet.get(..2)? != id.to_be_bytes() || !is_reply || failed {
        return None;
    }
    let count = |at: usize| Some(u16::from_be_bytes([*packet.get(at)?, *packet.get(at + 1)?]) as usize);
    let (questions, answers) = (count(4)?, count(6)?);
    let mut at = HEADER;
    for record in 0..questions + answers {
        // A name is a run of labels ending in a zero, or in a two byte pointer.
        loop {
            match *packet.get(at)? {
                0 => break at += 1,
                length if length & 0xC0 == 0xC0 => break at += 2,
                length => at += length as usize + 1,
            }
        }
        if record < questions {
            at += 4;
            continue;
        }
        let kind = count(at)?;
        let length = count(at + 8)?;
        at += 10;
        let data = packet.get(at..at + length)?;
        if kind == 1 && length == 4 {
            return data.try_into().ok();
        }
        at += length;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_bytes() {
        let mut packet = [0xFFu8; 64];
        let n = query(&mut packet, "api.muse.ai", 0x1234).unwrap();
        assert_eq!(
            &packet[..n],
            b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x03api\x04muse\x02ai\x00\x00\x01\x00\x01"
        );
    }

    #[test]
    fn rejects_bad_names_and_small_buffers() {
        let mut packet = [0u8; 64];
        assert!(query(&mut packet, "", 1).is_none());
        assert!(query(&mut packet, "a..b", 1).is_none());
        assert!(query(&mut packet[..20], "api.muse.ai", 1).is_none());
    }

    #[test]
    fn reads_an_address_past_a_cname() {
        let mut packet = [0u8; 128];
        let n = query(&mut packet, "a.io", 7).unwrap();
        packet[2] = 0x81;
        packet[7] = 2;
        let cname = b"\xC0\x0C\x00\x05\x00\x01\x00\x00\x00\x3C\x00\x03\x01b\x00";
        let address = b"\xC0\x0C\x00\x01\x00\x01\x00\x00\x00\x3C\x00\x04\x0A\x14\x1E\x28";
        packet[n..n + cname.len()].copy_from_slice(cname);
        packet[n + cname.len()..n + cname.len() + address.len()].copy_from_slice(address);
        let end = n + cname.len() + address.len();
        assert_eq!(answer(&packet[..end], 7), Some([10, 20, 30, 40]));
        assert_eq!(answer(&packet[..end], 8), None, "wrong id");
        assert_eq!(answer(&packet[..end - 2], 7), None, "cut short");
    }

    #[test]
    fn a_server_failure_is_no_answer() {
        let mut packet = [0u8; 64];
        let n = query(&mut packet, "a.io", 7).unwrap();
        packet[2] = 0x81;
        packet[3] = 2;
        assert_eq!(answer(&packet[..n], 7), None);
    }
}
