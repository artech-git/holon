use proptest::prelude::*;
use txp_core::{LogRecord, Lsn, ParticipantId, TxId};
use txp_wal::format::{decode_record, encode_record, Frame};

fn arb_record() -> impl Strategy<Value = LogRecord> {
    prop_oneof![
        any::<u128>().prop_map(|t| LogRecord::Abort { txid: TxId(t) }),
        any::<u128>().prop_map(|t| LogRecord::Done { txid: TxId(t) }),
        (any::<u128>(), "[a-z]{0,8}").prop_map(|(t, p)| LogRecord::Commit { txid: TxId(t), participants: vec![ParticipantId::new(p)] }),
        (any::<u128>(), "[a-z ]{0,32}").prop_map(|(t, n)| LogRecord::Begin { txid: TxId(t), name: n, manifest_digest: "x".into(), submitter: None, participants: vec![] }),
    ]
}

proptest! {
    #[test]
    fn roundtrip(lsn in any::<u64>(), rec in arb_record()) {
        let bytes = encode_record(Lsn(lsn), &rec);
        match decode_record(&bytes) {
            Frame::Ok { lsn: l, rec: r, consumed } => {
                prop_assert_eq!(l, Lsn(lsn));
                prop_assert_eq!(r, rec);
                prop_assert_eq!(consumed, bytes.len());
            }
            other => prop_assert!(false, "unexpected {:?}", other),
        }
    }

    #[test]
    fn any_prefix_or_flip_is_bad_or_end(lsn in any::<u64>(), rec in arb_record(), cut in 0usize..200, flip in 0usize..200) {
        let mut bytes = encode_record(Lsn(lsn), &rec);
        let cut = cut.min(bytes.len() - 1);
        bytes.truncate(cut);
        if let Frame::Ok { .. } = decode_record(&bytes) { prop_assert!(false, "truncated frame decoded") }
        let mut bytes = encode_record(Lsn(lsn), &rec);
        let i = flip % bytes.len();
        bytes[i] ^= 0x5a;
        if let Frame::Ok { lsn: l, rec: r, .. } = decode_record(&bytes) { prop_assert!(l == Lsn(lsn) && r == rec, "flip accepted with different content") }
    }
}
