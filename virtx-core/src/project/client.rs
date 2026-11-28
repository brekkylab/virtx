//! The client's end of `/projects/` and the members of each project.

use super::{
    ProjectCreateArgs, ProjectListOutputItem, ProjectMemberListOutputItem, ProjectMemberSetArgs,
};
use http_over_x::segment;

use crate::{Client, Error};

/// The path of the project `name` names: a project of the user's by name, or any by its id.
fn project(name: &str) -> String {
    format!("/projects/{}", segment(name))
}

/// Every project the client has a role in.
pub(crate) async fn list(client: &Client) -> Result<Vec<ProjectListOutputItem>, Error> {
    Ok(client.wire().get("/projects/", client.headers()).await?)
}

/// Make a project as `args` says, the client its owner.
pub(crate) async fn create(
    client: &Client,
    args: &ProjectCreateArgs,
) -> Result<ProjectListOutputItem, Error> {
    Ok(client
        .wire()
        .post("/projects/", client.headers(), args)
        .await?)
}

/// The project `name`, with the client's role in it.
pub(crate) async fn get(client: &Client, name: &str) -> Result<ProjectListOutputItem, Error> {
    Ok(client.wire().get(&project(name), client.headers()).await?)
}

/// Delete the project `name` and everything in it.
pub(crate) async fn remove(client: &Client, name: &str) -> Result<(), Error> {
    Ok(client
        .wire()
        .delete(&project(name), client.headers())
        .await?)
}

/// The members of the project `name`.
pub(crate) async fn list_member(
    client: &Client,
    name: &str,
) -> Result<Vec<ProjectMemberListOutputItem>, Error> {
    Ok(client
        .wire()
        .get(&format!("{}/members/", project(name)), client.headers())
        .await?)
}

/// Give `user` the role `args` says in the project `name`.
pub(crate) async fn set_member(
    client: &Client,
    name: &str,
    user: &str,
    args: &ProjectMemberSetArgs,
) -> Result<ProjectMemberListOutputItem, Error> {
    let path = format!("{}/members/{}", project(name), segment(user));
    Ok(client.wire().put(&path, client.headers(), args).await?)
}

/// Take `user`'s role in the project `name` away.
pub(crate) async fn remove_member(client: &Client, name: &str, user: &str) -> Result<(), Error> {
    Ok(client
        .wire()
        .delete(
            &format!("{}/members/{}", project(name), segment(user)),
            client.headers(),
        )
        .await?)
}
