use std::sync::Arc;

use hashtree_network::{
    clear_channel_registry, MeshRouter, MockConnectionFactory, MockRelay, MockRelayTransport,
    PoolConfig, PoolSettings, SignalingTransport,
};

type TestRouter = MeshRouter<MockRelayTransport, MockConnectionFactory>;

async fn drain_router_messages(
    routers: &[(&Arc<MockRelayTransport>, &Arc<TestRouter>)],
    max_passes: usize,
) {
    for _ in 0..max_passes {
        let mut progressed = false;
        for (transport, router) in routers {
            while let Some(msg) = transport.try_recv() {
                router
                    .handle_message(msg)
                    .await
                    .expect("handle router message");
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
        tokio::task::yield_now().await;
    }
}

fn test_router(
    relay: &Arc<MockRelay>,
    peer_id: &str,
    pools: PoolSettings,
) -> (Arc<MockRelayTransport>, Arc<TestRouter>) {
    let transport = Arc::new(relay.create_transport(peer_id.to_string()));
    let factory = Arc::new(MockConnectionFactory::new(peer_id.to_string(), 0));
    let router = Arc::new(MeshRouter::new(
        peer_id.to_string(),
        transport.clone(),
        factory,
        pools,
        false,
    ));
    (transport, router)
}

#[tokio::test]
async fn peer_router_sim_forms_connection_between_two_nodes() {
    clear_channel_registry().await;

    let relay = MockRelay::new();
    let pools = PoolSettings {
        follows: PoolConfig {
            max_connections: 4,
            satisfied_connections: 1,
        },
        other: PoolConfig {
            max_connections: 4,
            satisfied_connections: 1,
        },
    };

    let (transport_a, router_a) = test_router(&relay, "1", pools.clone());
    let (transport_b, router_b) = test_router(&relay, "2", pools);

    transport_a.connect(&[]).await.expect("connect a");
    transport_b.connect(&[]).await.expect("connect b");

    router_a
        .send_hello(vec!["root-a".to_string()])
        .await
        .expect("hello a");
    router_b
        .send_hello(vec!["root-b".to_string()])
        .await
        .expect("hello b");

    drain_router_messages(&[(&transport_a, &router_a), (&transport_b, &router_b)], 16).await;

    assert_eq!(router_a.peer_count().await, 1);
    assert_eq!(router_b.peer_count().await, 1);
    assert!(router_a.get_channel("2").await.is_some());
    assert!(router_b.get_channel("1").await.is_some());
}

#[tokio::test]
async fn peer_router_sim_respects_pool_capacity() {
    clear_channel_registry().await;

    let relay = MockRelay::new();
    let saturated = PoolSettings {
        follows: PoolConfig {
            max_connections: 1,
            satisfied_connections: 0,
        },
        other: PoolConfig {
            max_connections: 1,
            satisfied_connections: 0,
        },
    };

    let (transport_a, router_a) = test_router(&relay, "1", saturated.clone());
    let (transport_b, router_b) = test_router(&relay, "2", saturated.clone());
    let (transport_c, router_c) = test_router(&relay, "3", saturated);

    transport_a.connect(&[]).await.expect("connect a");
    transport_b.connect(&[]).await.expect("connect b");
    transport_c.connect(&[]).await.expect("connect c");

    router_a.send_hello(vec![]).await.expect("hello a");
    router_b.send_hello(vec![]).await.expect("hello b");
    router_c.send_hello(vec![]).await.expect("hello c");

    drain_router_messages(
        &[
            (&transport_a, &router_a),
            (&transport_b, &router_b),
            (&transport_c, &router_c),
        ],
        24,
    )
    .await;

    assert!(router_a.peer_count().await <= 1);
    assert!(router_b.peer_count().await <= 1);
    assert!(router_c.peer_count().await <= 1);
}
