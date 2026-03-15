use crate::{
    cardinal_core::Reaction,
    g_rpc::storage::{add_queue, remove_queue},
};

pub async fn force_reaction(reaction: Reaction) {
    let recv = reaction.clone();

    let _result = add_queue(&recv.trace_id, &recv.r#type).await;
}

pub async fn revoke_force(agent: &str) {
    let _result = remove_queue(agent).await;
}
