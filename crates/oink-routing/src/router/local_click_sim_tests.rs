// Local-click model coverage for packet loss, reordering and duplication.
#[test]
fn sim_three_nodes_drop_reorder_duplicate_and_repair() {
    for seed in 0..32u64 {
        let mut g = group();
        click(&mut g[1]);
        click(&mut g[2]);
        let mut random = seed + 1;
        for round in 0..20 {
            let mut pending = Vec::new();
            for r in &mut g {
                let from = r.me();
                for o in std::mem::take(&mut r.out) {
                    if let Output::Send { to, msg, .. } = o {
                        pending.push((from, to, msg));
                    }
                }
            }
            pending.reverse();
            for (from, to, msg) in pending {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                if round < 5 && random % 3 == 0 {
                    continue;
                }
                let target = g.iter_mut().find(|r| r.me() == to).unwrap();
                target.input_message(from, msg.clone(), round);
                target.input_message(from, msg, round);
            }
            // Model the existing transport state snapshot delivery after loss.
            for i in 0..3 {
                for j in 0..3 {
                    if i != j {
                        let from = g[i].me();
                        for msg in g[i].local_click_repair(g[j].me()) {
                            g[j].input_message(from, msg, round);
                        }
                    }
                }
            }
        }
        pump(&mut g, true);
        assert!(
            g.iter().all(|r| r.focus_claim() == (11, DeviceId([3; 32]))),
            "seed {seed}: {:?}",
            g.iter().map(Router::focus_claim).collect::<Vec<_>>()
        );
        assert!(g.iter().all(|r| !r.local_click_reserved()), "seed {seed}");
    }
}
