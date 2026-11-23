#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    struct MockRpc {
        sent: Arc<Mutex<Vec<(String, u64)>>>,
    }

    impl MockRpc {
        fn send_once(&self, wallet: &str, amount: u64) -> Option<String> {
            let mut sent = self.sent.lock().unwrap();
            if sent.iter().any(|(address, paid)| address == wallet && *paid == amount) {
                return None;
            }
            sent.push((wallet.to_string(), amount));
            Some(format!("0xtest-{}-{}", wallet.len(), amount))
        }
    }

    #[test]
    fn mock_rpc_payout_is_not_sent_twice() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let rpc = MockRpc {
            sent: sent.clone(),
        };
        let wallet = "0x1111111111111111111111111111111111111111";

        assert!(rpc.send_once(wallet, 11_700).is_some());
        assert!(rpc.send_once(wallet, 11_700).is_none());
        assert_eq!(sent.lock().unwrap().len(), 1);
    }
}
