//! The client's end of `/projects/` and the members of each project.

use super::{
    ProjectCreateArgs, ProjectListOutputItem, ProjectMemberListOutputItem, ProjectMemberSetArgs,
};
use crate::{
    Error,
    transport::{TransportClient, http, segment},
};

/// The path of the project `name` names: a project of the user's by name, or any by its id.
fn project(name: &str) -> String {
    format!("/projects/{}", segment(name))
}

/// Every project the client has a role in.
pub(crate) async fn list(client: &TransportClient) -> Result<Vec<ProjectListOutputItem>, Error> {
    http::get(client, "/projects/").await
}

/// Make a project as `args` says, the client its owner.
pub(crate) async fn create(
    client: &TransportClient,
    args: &ProjectCreateArgs,
) -> Result<ProjectListOutputItem, Error> {
    http::post(client, "/projects/", args).await
}

/// The project `name`, with the client's role in it.
pub(crate) async fn get(
    client: &TransportClient,
    name: &str,
) -> Result<ProjectListOutputItem, Error> {
    http::get(client, &project(name)).await
}

/// Delete the project `name` and everything in it.
pub(crate) async fn remove(client: &TransportClient, name: &str) -> Result<(), Error> {
    http::delete(client, &project(name)).await
}

/// The members of the project `name`.
pub(crate) async fn list_member(
    client: &TransportClient,
    name: &str,
) -> Result<Vec<ProjectMemberListOutputItem>, Error> {
    http::get(client, &format!("{}/members/", project(name))).await
}

/// Give `user` the role `args` says in the project `name`.
pub(crate) async fn set_member(
    client: &TransportClient,
    name: &str,
    user: &str,
    args: &ProjectMemberSetArgs,
) -> Result<ProjectMemberListOutputItem, Error> {
    let path = format!("{}/members/{}", project(name), segment(user));
    http::put(client, &path, args).await
}

/// Take `user`'s role in the project `name` away.
pub(crate) async fn remove_member(
    client: &TransportClient,
    name: &str,
    user: &str,
) -> Result<(), Error> {
    http::delete(
        client,
        &format!("{}/members/{}", project(name), segment(user)),
    )
    .await
}
