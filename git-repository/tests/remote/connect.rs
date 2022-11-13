#[cfg(any(feature = "blocking-network-client", feature = "async-network-client-async-std"))]
mod blocking_or_async_io {
    mod protocol_allow {
        use git_features::progress;
        use git_repository as git;
        use git_repository::remote::Direction::Fetch;
        use serial_test::serial;

        use crate::remote;

        #[maybe_async::test(
            feature = "blocking-network-client",
            async(feature = "async-network-client-async-std", async_std::test)
        )]
        async fn deny() {
            for name in ["protocol_denied", "protocol_file_denied"] {
                let repo = remote::repo(name);
                let remote = repo.find_remote("origin").unwrap();
                assert!(matches!(
                    remote.connect(Fetch, progress::Discard).await.err(),
                    Some(git::remote::connect::Error::ProtocolDenied {
                        url: _,
                        scheme: git::url::Scheme::File
                    })
                ));
            }
        }

        #[maybe_async::test(
            feature = "blocking-network-client",
            async(feature = "async-network-client-async-std", async_std::test)
        )]
        #[serial]
        async fn user() -> crate::Result {
            for (env_value, should_allow) in [(None, true), (Some("0"), false), (Some("1"), true)] {
                let _env = env_value.map(|value| git_testtools::Env::new().set("GIT_PROTOCOL_FROM_USER", value));
                let repo = git::open_opts(
                    remote::repo("protocol_file_user").git_dir(),
                    git::open::Options::isolated().permissions(git::Permissions {
                        env: git::permissions::Environment {
                            git_prefix: git_sec::Permission::Allow,
                            ..git::permissions::Environment::all()
                        },
                        ..git::Permissions::isolated()
                    }),
                )?;
                let remote = repo.find_remote("origin")?;
                assert_eq!(
                    remote.connect(Fetch, progress::Discard).await.is_ok(),
                    should_allow,
                    "Value = {:?}",
                    env_value
                );
            }
            Ok(())
        }
    }
}
