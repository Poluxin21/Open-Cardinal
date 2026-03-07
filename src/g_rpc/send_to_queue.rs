use crate::{cardinal_core::Reaction, g_rpc::storage::{add_queue, remove_queue}};

pub async fn force_reaction(reaction: Reaction) -> Result<(), Box<dyn std::error::Error>> {
    let recv = reaction.clone();
    
    let _ = add_queue(&recv.trace_id, &recv.r#type);

    Ok(())
}

pub async fn revoke_force(agent: &str) -> Result<(), Box<dyn std::error::Error>> {
    let _ = remove_queue(agent);

    Ok(())
}