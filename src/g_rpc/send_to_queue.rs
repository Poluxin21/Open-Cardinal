use tonic::Request;

use crate::cardinal_core::Reaction;

pub async fn force_reaction(reaction: Reaction) -> Result<(), Box<dyn std::error::Error>> {
    
    let request = Request::new(reaction);

    


    Ok(())
}