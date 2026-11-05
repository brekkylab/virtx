//! An image's life: pulled or built, listed, tagged, downloaded and uploaded back, untagged,
//! pushed and pruned, and jobs at once on one reference or beside a prune.

use futures_util::TryStreamExt;
use virtx_core::{ImageJobEnd, ImageJobEntry, Recipe, RegistryAuth, SandboxCreateArgs, Step};

use crate::harness::{
    ALPINE, Made, Run, alpine, build, image_digest, image_end, output, refused, tags, temporary,
    unique,
};

scenarios![images:
    Any a_pull_ends_with_the_digest_its_reference_points_at,
    Any a_build_points_its_reference_at_what_it_built,
    Any an_image_downloaded_uploads_back_as_it_was,
    Any untagging_takes_that_tag_alone,
    Any jobs_tagging_one_reference_at_once_leave_it_on_one_of_theirs,
    Prune a_prune_keeps_what_a_tag_or_a_sandbox_holds,
    Prune a_prune_while_a_build_runs_takes_nothing_it_built,
    Test a_push_ends_with_the_digest_it_sent_and_shows_no_credentials,
    Test a_cancelled_image_job_changes_no_tag,
];

/// A recipe from [`ALPINE`] whose step writes `text`, so its image is no other's.
fn writing(text: &str) -> Recipe {
    Recipe::new(ALPINE).step(Step::run(format!("echo {text} > /built")))
}

async fn jobs_tagging_one_reference_at_once_leave_it_on_one_of_theirs(run: Run) {
    let client = run.client().await;
    let (first, second) = tokio::join!(
        client.pull_image(ALPINE, None),
        client.pull_image(ALPINE, None)
    );
    let first = image_digest(&client, &first.unwrap().id).await;
    let second = image_digest(&client, &second.unwrap().id).await;
    assert_eq!(first, second);
    assert_eq!(tags(&client).await.get(ALPINE), Some(&first));

    let reference = format!("{}:latest", unique("both"));
    run.made(Made::Tag(reference.clone()));
    let (one, two) = tokio::join!(
        client.build_image(writing("one"), &reference),
        client.build_image(writing("two"), &reference)
    );
    let one = image_digest(&client, &one.unwrap().id).await;
    let two = image_digest(&client, &two.unwrap().id).await;
    assert_ne!(one, two);
    // Whichever ended last moved the tag last.
    let tagged = tags(&client).await.get(&reference).cloned();
    assert!(
        tagged == Some(one.clone()) || tagged == Some(two.clone()),
        "{reference} is on {tagged:?}, not {one} or {two}"
    );
    client.remove_image(&reference).await.unwrap();
    client.close().await;
}

async fn a_prune_while_a_build_runs_takes_nothing_it_built(run: Run) {
    let client = run.client().await;
    alpine(&client).await;
    let reference = format!("{}:latest", unique("pruned"));
    run.made(Made::Tag(reference.clone()));
    let job = client
        .build_image(writing(&reference), &reference)
        .await
        .unwrap();
    // Another server builds for a while, and prunes while it does; the test server's build
    // has ended by now, so against it the two do not meet.
    client.prune_image().await.unwrap();
    let end = image_end(&client, &job.id).await;
    match end.digest {
        // What it built is whole: every blob of it is there to download.
        Some(digest) => {
            assert_eq!(tags(&client).await.get(&reference), Some(&digest));
            let path = temporary("pruned.tar");
            let downloaded = output(client.download_image(&reference, &path)).await;
            assert_eq!(downloaded.digest, digest);
            std::fs::remove_file(&path).unwrap();
            client.remove_image(&reference).await.unwrap();
        }
        // A server may fail it, but then it tags nothing.
        None => assert!(
            !tags(&client).await.contains_key(&reference),
            "failed, {:?}, and tagged still",
            end.error
        ),
    }
    client.close().await;
}

async fn a_pull_ends_with_the_digest_its_reference_points_at(run: Run) {
    let client = run.client().await;
    let job = client.pull_image(ALPINE, None).await.unwrap();
    let digest = image_end(&client, &job.id).await.digest.expect("pulled");
    let read = client.get_image_job(&job.id).await.unwrap();
    assert!(!read.running);
    assert_eq!(read.digest.as_ref(), Some(&digest));
    assert_eq!(tags(&client).await.get(ALPINE), Some(&digest));
    client.close().await;
}

async fn a_build_points_its_reference_at_what_it_built(run: Run) {
    let client = run.client().await;
    let reference = format!("{}:latest", unique("build"));
    let digest = build(&run, &client, &reference).await;
    assert_eq!(tags(&client).await.get(&reference), Some(&digest));
    client.remove_image(&reference).await.unwrap();
    client.close().await;
}

async fn an_image_downloaded_uploads_back_as_it_was(run: Run) {
    let client = run.client().await;
    let digest = alpine(&client).await;
    let reference = format!("{}:copy", unique("copy"));
    run.made(Made::Tag(reference.clone()));
    client.tag_image(ALPINE, &reference).await.unwrap();
    let path = temporary("image.tar");

    let downloaded = output(client.download_image(&reference, &path)).await;
    assert_eq!(downloaded.digest, digest);
    client.remove_image(&reference).await.unwrap();
    assert!(!tags(&client).await.contains_key(&reference));

    let uploaded = output(client.upload_image(&path, &reference)).await;
    assert_eq!(uploaded.digest, digest);
    assert_eq!(tags(&client).await.get(&reference), Some(&digest));

    client.remove_image(&reference).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    client.close().await;
}

async fn untagging_takes_that_tag_alone(run: Run) {
    let client = run.client().await;
    let digest = alpine(&client).await;
    let name = unique("tag");
    let (kept, untagged) = (format!("{name}:kept"), format!("{name}:untagged"));
    run.made(Made::Tag(kept.clone()));
    run.made(Made::Tag(untagged.clone()));
    client.tag_image(ALPINE, &kept).await.unwrap();
    client.tag_image(ALPINE, &untagged).await.unwrap();

    client.untag_image(&untagged).await.unwrap();
    let tags = tags(&client).await;
    assert_eq!(tags.get(&kept), Some(&digest));
    assert_eq!(tags.get(ALPINE), Some(&digest));
    assert!(!tags.contains_key(&untagged));
    client.untag_image(&kept).await.unwrap();
    client.close().await;
}

async fn a_prune_keeps_what_a_tag_or_a_sandbox_holds(run: Run) {
    let client = run.client().await;
    let alpine = alpine(&client).await;
    let (loose, booted) = (
        format!("{}:latest", unique("loose")),
        format!("{}:latest", unique("booted")),
    );
    let loose_digest = build(&run, &client, &loose).await;
    let booted_digest = build(&run, &client, &booted).await;
    let sandbox = run
        .create_sandbox(&client, &SandboxCreateArgs::new(booted.as_str()))
        .await;
    client.untag_image(&loose).await.unwrap();
    client.untag_image(&booted).await.unwrap();

    let pruned = client.prune_image().await.unwrap();
    assert!(!pruned.images.contains(&alpine), "{pruned:?}");
    assert!(!pruned.images.contains(&booted_digest), "{pruned:?}");
    // A server may leave what is too new to tell from what a job will tag: only the test
    // server is sure to take it now.
    if run.is_test() {
        assert!(pruned.images.contains(&loose_digest), "{pruned:?}");
        assert!(pruned.reclaimed > 0);
    }

    client.remove_sandbox(&sandbox.id).await.unwrap();
    if run.is_test() {
        let pruned = client.prune_image().await.unwrap();
        assert_eq!(pruned.images, [booted_digest]);
    }
    assert_eq!(tags(&client).await.get(ALPINE), Some(&alpine));
    client.close().await;
}

async fn a_push_ends_with_the_digest_it_sent_and_shows_no_credentials(run: Run) {
    let client = run.client().await;
    let digest = alpine(&client).await;
    let auth = RegistryAuth::Bearer {
        token: "e2e-secret".into(),
    };
    let job = client.push_image(ALPINE, Some(auth)).await.unwrap();
    let log: Vec<_> = client
        .image_job_log(&job.id, None)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(
        log.last().map(|entry| &entry.entry),
        Some(&ImageJobEntry::End(ImageJobEnd {
            digest: Some(digest),
            error: None,
        }))
    );
    let shown = serde_json::to_string(&(client.get_image_job(&job.id).await.unwrap(), log));
    assert!(!shown.unwrap().contains("e2e-secret"));

    let missing = format!("{}:latest", unique("unpushed"));
    let job = client.push_image(&missing, None).await.unwrap();
    assert!(image_end(&client, &job.id).await.error.is_some());
    client.close().await;
}

async fn a_cancelled_image_job_changes_no_tag(run: Run) {
    let client = run.client().await;
    let before = tags(&client).await;
    let job = client.pull_image("hang", None).await.unwrap();
    assert!(job.running);
    client.remove_image_job(&job.id).await.unwrap();
    assert_eq!(refused(client.get_image_job(&job.id).await).0, 404);
    assert_eq!(tags(&client).await, before);
    client.close().await;
}
