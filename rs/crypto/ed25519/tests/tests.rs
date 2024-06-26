use hex_literal::hex;
use ic_crypto_ed25519::*;
use rand::Rng;

#[test]
fn secret_key_serialization_round_trips() {
    let mut rng = &mut rand::thread_rng();
    for _ in 0..100 {
        let key = SecretKey::generate_key(&mut rng);

        let via_raw = SecretKey::deserialize_raw(&key.serialize_raw()).unwrap();
        assert_eq!(key, via_raw);

        let via_pkcs8 = SecretKey::deserialize_pkcs8(&key.serialize_pkcs8()).unwrap();
        assert_eq!(key, via_pkcs8);

        let via_pem = SecretKey::deserialize_pkcs8_pem(&key.serialize_pkcs8_pem()).unwrap();
        assert_eq!(key, via_pem);
    }
}

#[test]
fn pkcs8_rep_includes_the_public_key() {
    let mut rng = &mut rand::thread_rng();
    let sk = SecretKey::generate_key(&mut rng);
    let pk = sk.public_key().serialize_raw();

    let sk_pkcs8 = sk.serialize_pkcs8();

    let pk_offset = sk_pkcs8.len() - pk.len();

    assert_eq!(hex::encode(pk), hex::encode(&sk_pkcs8[pk_offset..]));
}

#[test]
fn signatures_we_generate_will_verify() {
    let mut rng = &mut rand::thread_rng();
    for _ in 0..100 {
        let sk = SecretKey::generate_key(&mut rng);
        let pk = sk.public_key();

        let msg = rng.gen::<[u8; 32]>();

        let sig = sk.sign_message(&msg);
        assert_eq!(sig.len(), 64);

        assert!(pk.verify_signature(&msg, &sig).is_ok());
    }
}

#[test]
fn public_key_deserialization_rejects_key_not_on_curve() {
    let invalid_pk = hex!("0200000000000000000000000000000000000000000000000000000000000000");
    assert!(PublicKey::deserialize_raw(&invalid_pk).is_err());
}

#[test]
fn public_key_deserialization_rejects_keys_of_incorrect_length() {
    for len in 0..128 {
        if len == PublicKey::BYTES {
            continue;
        }

        let buf = vec![2; len];

        assert!(PublicKey::deserialize_raw(&buf).is_err());
    }
}

#[test]
fn batch_verification_works() {
    fn batch_verifies(msg: &[[u8; 32]], sigs: &[Vec<u8>], keys: &[PublicKey]) -> bool {
        let msg_ref = msg.iter().map(|m| m.as_ref()).collect::<Vec<&[u8]>>();
        let sig_ref = sigs.iter().map(|s| s.as_ref()).collect::<Vec<&[u8]>>();
        PublicKey::batch_verify(&msg_ref, &sig_ref, keys).is_ok()
    }

    // Return two distinct positions in [0..max)
    fn two_positions<R: Rng>(max: usize, rng: &mut R) -> (usize, usize) {
        assert!(max > 1);

        let pos0 = rng.gen::<usize>() % max;

        loop {
            let pos1 = rng.gen::<usize>() % max;
            if pos0 != pos1 {
                return (pos0, pos1);
            }
        }
    }

    let mut rng = &mut rand::thread_rng();

    for batch_size in 1..15 {
        let sk = (0..batch_size)
            .map(|_| SecretKey::generate_key(&mut rng))
            .collect::<Vec<_>>();
        let mut pk = sk.iter().map(|k| k.public_key()).collect::<Vec<_>>();

        let mut msg = (0..batch_size)
            .map(|_| rng.gen::<[u8; 32]>())
            .collect::<Vec<_>>();
        let mut sigs = (0..batch_size)
            .map(|i| sk[i].sign_message(&msg[i]))
            .collect::<Vec<_>>();

        assert!(batch_verifies(&msg, &sigs, &pk));

        // Corrupt a random signature and check that the batch fails:
        let corrupted_sig_idx = rng.gen::<usize>() % batch_size;
        let corrupted_sig_byte = rng.gen::<usize>() % 64;
        let corrupted_sig_mask = std::cmp::max(1, rng.gen::<u8>());
        sigs[corrupted_sig_idx][corrupted_sig_byte] ^= corrupted_sig_mask;
        assert!(!batch_verifies(&msg, &sigs, &pk));

        // Uncorrupt the signature, then corrupt a random message, verify it fails:
        sigs[corrupted_sig_idx][corrupted_sig_byte] ^= corrupted_sig_mask;
        // We fixed the signature so the batch should verify again:
        debug_assert!(batch_verifies(&msg, &sigs, &pk));

        let corrupted_msg_idx = rng.gen::<usize>() % batch_size;
        let corrupted_msg_byte = rng.gen::<usize>() % 32;
        let corrupted_msg_mask = std::cmp::max(1, rng.gen::<u8>());
        msg[corrupted_msg_idx][corrupted_msg_byte] ^= corrupted_msg_mask;
        assert!(!batch_verifies(&msg, &sigs, &pk));

        // Fix the corrupted message
        msg[corrupted_msg_idx][corrupted_msg_byte] ^= corrupted_msg_mask;

        if batch_size > 1 {
            // Swapping a key causes batch verification to fail:
            let (swap0, swap1) = two_positions(batch_size, rng);
            pk.swap(swap0, swap1);
            assert!(!batch_verifies(&msg, &sigs, &pk));

            // If we swap (also) the message, verification still fails:
            msg.swap(swap0, swap1);
            assert!(!batch_verifies(&msg, &sigs, &pk));

            // If we swap the signature so it is consistent, batch is accepted:
            sigs.swap(swap0, swap1);
            assert!(batch_verifies(&msg, &sigs, &pk));
        }
    }
}

#[test]
fn test_der_public_key_conversions() {
    let test_data = [
        (hex!("b3997656ba51ff6da37b61d8d549ec80717266ecf48fb5da52b654412634844c"),
         hex!("302a300506032b6570032100b3997656ba51ff6da37b61d8d549ec80717266ecf48fb5da52b654412634844c")),
        (hex!("a5afb5feb6dfb6ddf5dd6563856fff5484f5fe304391d9ed06697861f220c610"),
         hex!("302a300506032b6570032100a5afb5feb6dfb6ddf5dd6563856fff5484f5fe304391d9ed06697861f220c610")),
    ];

    for (raw, der) in &test_data {
        let pk_raw = PublicKey::deserialize_raw(raw).unwrap();

        let pk_der = PublicKey::deserialize_rfc8410_der(der).unwrap();

        assert_eq!(pk_raw, pk_der);
        assert_eq!(pk_raw.serialize_rfc8410_der(), der);
        assert_eq!(pk_der.serialize_raw(), *raw);
    }
}

#[test]
fn can_parse_pkcs8_v1_der_secret_key() {
    let pkcs8_v1 = hex!("302e020100300506032b657004220420d4ee72dbf913584ad5b6d8f1f769f8ad3afe7c28cbf1d4fbe097a88f44755842");
    let sk = SecretKey::deserialize_pkcs8(&pkcs8_v1).unwrap();

    assert_eq!(
        hex::encode(sk.serialize_raw()),
        "d4ee72dbf913584ad5b6d8f1f769f8ad3afe7c28cbf1d4fbe097a88f44755842"
    );
}

#[test]
fn can_parse_pkcs8_v2_der_secret_key() {
    // From ring test data
    let pkcs8_v2 = hex!("3051020101300506032b657004220420d4ee72dbf913584ad5b6d8f1f769f8ad3afe7c28cbf1d4fbe097a88f4475584281210019bf44096984cdfe8541bac167dc3b96c85086aa30b6b6cb0c5c38ad703166e1");

    let sk = SecretKey::deserialize_pkcs8(&pkcs8_v2).unwrap();

    assert_eq!(
        hex::encode(sk.serialize_raw()),
        "d4ee72dbf913584ad5b6d8f1f769f8ad3afe7c28cbf1d4fbe097a88f44755842"
    );
}

#[test]
fn should_pass_wycheproof_test_vectors() {
    let test_set = wycheproof::eddsa::TestSet::load(wycheproof::eddsa::TestName::Ed25519)
        .expect("Unable to load tests");

    for test_group in test_set.test_groups {
        let pk = PublicKey::deserialize_raw(&test_group.key.pk).unwrap();

        for test in test_group.tests {
            let accept = pk.verify_signature(&test.msg, &test.sig).is_ok();

            if test.result == wycheproof::TestResult::Valid {
                assert!(accept);
            } else {
                assert!(!accept);
            }
        }
    }
}
