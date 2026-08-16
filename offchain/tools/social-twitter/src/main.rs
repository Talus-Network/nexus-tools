//! # `xyz.taluslabs.social.twitter.*`
//!
//! This module contains tools for Twitter operations.
#![doc = include_str!("../README.md")]
#![recursion_limit = "256"]
#![allow(
    clippy::single_component_path_imports,
    clippy::large_enum_variant,
    clippy::upper_case_acronyms,
    clippy::too_many_arguments,
    clippy::manual_clamp,
    clippy::derivable_impls,
    clippy::to_string_trait_impl,
    clippy::assertions_on_constants
)]

use nexus_toolkit::bootstrap;
mod auth;
mod direct_message;
mod error;
mod list;
mod media;
mod tweet;
mod twitter_client;
mod user;

macro_rules! social_tools {
    ($consumer:ident) => {
        $consumer!([
            crate::tweet::post_tweet::PostTweet,
            crate::tweet::delete_tweet::DeleteTweet,
            crate::tweet::get_tweet::GetTweet,
            crate::tweet::like_tweet::LikeTweet,
            crate::tweet::get_mentioned_tweets::GetMentionedTweets,
            crate::tweet::get_user_tweets::GetUserTweets,
            crate::tweet::get_recent_tweet_count::GetRecentTweetCount,
            crate::tweet::get_recent_search_tweets::GetRecentSearchTweets,
            crate::tweet::unlike_tweet::UnlikeTweet,
            crate::tweet::undo_retweet_tweet::UndoRetweetTweet,
            crate::tweet::get_tweets::GetTweets,
            crate::tweet::retweet_tweet::RetweetTweet,
            crate::list::create_list::CreateList,
            crate::list::delete_list::DeleteList,
            crate::list::get_list::GetList,
            crate::list::get_list_tweets::GetListTweets,
            crate::list::get_list_members::GetListMembers,
            crate::list::update_list::UpdateList,
            crate::list::add_member::AddMember,
            crate::list::get_user_lists::GetUserLists,
            crate::list::remove_member::RemoveMember,
            crate::media::upload_media::UploadMedia,
            crate::user::get_user_by_id::GetUserById,
            crate::user::get_user_by_username::GetUserByUsername,
            crate::user::follow_user::FollowUser,
            crate::direct_message::send_message_to_group_conversation::SendMessageToGroupConversation,
            crate::direct_message::create_group_conversation::CreateGroupDmConversation,
            crate::direct_message::get_conversation_messages_by_id::GetConversationMessagesById,
            crate::direct_message::get_conversation_messages::GetConversationMessages,
            crate::direct_message::send_direct_message::SendDirectMessage,
            crate::user::unfollow_user::UnfollowUser,
            crate::user::get_users_by_username::GetUsersByUsername,
            crate::user::get_users_by_id::GetUsersById,
        ]);
    };
}

/// This function bootstraps the tool and starts the server.
#[tokio::main]
async fn main() {
    social_tools!(bootstrap);
}

#[cfg(test)]
mod protocol_tests {
    use {nexus_sdk::types::ToolMeta, nexus_toolkit::NexusTool};

    fn assert_protocol_compatible<T: NexusTool>() {
        let meta = ToolMeta {
            fqn: T::fqn(),
            url: String::new(),
            description: T::description().to_string(),
            timeout: T::timeout(),
            input_schema: serde_json::to_vec(&schemars::schema_for!(T::Input)).unwrap(),
            output_schema: serde_json::to_vec(&schemars::schema_for!(T::Output)).unwrap(),
        };

        meta.meta_schema().unwrap_or_else(|error| {
            panic!("{} metadata is not protocol-compatible: {error}", T::fqn())
        });
    }

    macro_rules! assert_protocol_compatible_tools {
        ([$($tool:ty),+ $(,)?]) => {
            $(assert_protocol_compatible::<$tool>();)+
        };
    }

    #[test]
    fn all_registered_tools_have_protocol_compatible_metadata() {
        social_tools!(assert_protocol_compatible_tools);
    }
}
