//! Snapshots and volumes: held by the sandboxes made on them until those are removed, their
//! files kept, and a volume's mounts.

use virtx_core::{
    Error, FileRoot, MountSource, MountType, SandboxCreateArgs, SandboxVolume,
    VolumeMountListOutputItem,
};

use crate::harness::{ALPINE, Made, Run, alpine, refused, sandbox, unique};

scenarios![storage:
    Any a_sandbox_holds_its_snapshot_until_it_is_removed,
    Any a_sandbox_holds_the_volumes_it_mounts_until_it_is_removed,
    Test a_mount_shows_its_type_and_not_its_credentials,
];

async fn a_sandbox_holds_its_snapshot_until_it_is_removed(run: Run) {
    let client = run.client().await;
    let name = unique("snapshot");
    run.made(Made::Snapshot(name.clone()));
    let snapshot = client.create_snapshot(Some(&name), None).await.unwrap();
    assert!(!snapshot.in_use);
    let id = sandbox(
        &run,
        &client,
        SandboxCreateArgs {
            snapshot: Some(name.clone()),
            ..SandboxCreateArgs::new(ALPINE)
        },
    )
    .await;
    assert!(client.get_snapshot(&name).await.unwrap().in_use);
    assert_eq!(
        refused(client.remove_snapshot(&name).await),
        (409, "in_use".into())
    );
    let root = FileRoot::Snapshot(name.clone());
    assert!(matches!(
        client.read_dir(&root, "").await,
        Err(Error::Status(423, _))
    ));

    client.remove_sandbox(&id).await.unwrap();
    assert!(!client.get_snapshot(&name).await.unwrap().in_use);
    client.read_dir(&root, "").await.unwrap();
    client.remove_snapshot(&name).await.unwrap();
    assert_eq!(refused(client.get_snapshot(&name).await).0, 404);
    client.close().await;
}

async fn a_sandbox_holds_the_volumes_it_mounts_until_it_is_removed(run: Run) {
    let client = run.client().await;
    let volume = unique("volume");
    run.made(Made::Volume(volume.clone()));
    client.create_volume(Some(&volume)).await.unwrap();
    let root = FileRoot::Volume(volume.clone());
    client
        .write_file(&root, "kept.txt", "kept", None)
        .await
        .unwrap();
    let mount = |path: &str| SandboxVolume {
        volume: volume.clone(),
        path: path.into(),
        readonly: false,
    };

    let twice = SandboxCreateArgs {
        volumes: vec![mount("/data"), mount("/data")],
        ..SandboxCreateArgs::new(ALPINE)
    };
    alpine(&client).await;
    let made = client.create_sandbox(&twice).await;
    if let Ok(sandbox) = &made {
        run.made(Made::Sandbox(sandbox.id.clone()));
    }
    let (status, _) = refused(made);
    assert!((400..500).contains(&status), "{status}");

    let id = sandbox(
        &run,
        &client,
        SandboxCreateArgs {
            volumes: vec![mount("/data")],
            ..SandboxCreateArgs::new(ALPINE)
        },
    )
    .await;
    assert_eq!(
        refused(client.remove_volume(&volume).await),
        (409, "in_use".into())
    );
    client.remove_sandbox(&id).await.unwrap();
    assert_eq!(client.read_text(&root, "kept.txt").await.unwrap(), "kept");
    client.remove_volume(&volume).await.unwrap();
    assert_eq!(refused(client.get_volume(&volume).await).0, 404);
    client.close().await;
}

async fn a_mount_shows_its_type_and_not_its_credentials(run: Run) {
    let client = run.client().await;
    let volume = unique("mounted");
    run.made(Made::Volume(volume.clone()));
    client.create_volume(Some(&volume)).await.unwrap();
    let source = || MountSource::Notion {
        api_key: "e2e-secret".into(),
    };
    let mount = VolumeMountListOutputItem {
        path: "notion".into(),
        mount_type: MountType::Notion,
    };
    let made = client
        .create_volume_mount(&volume, "notion", source())
        .await
        .unwrap();
    assert_eq!(made, mount);
    let listed = client.list_volume_mount(&volume).await.unwrap();
    assert_eq!(listed, [mount]);
    assert!(
        !serde_json::to_string(&listed)
            .unwrap()
            .contains("e2e-secret")
    );
    let again = client
        .create_volume_mount(&volume, "notion", source())
        .await;
    assert_eq!(refused(again), (409, "exists".into()));
    client.remove_volume_mount(&volume, "notion").await.unwrap();
    assert!(client.list_volume_mount(&volume).await.unwrap().is_empty());
    client.remove_volume(&volume).await.unwrap();
    client.close().await;
}
