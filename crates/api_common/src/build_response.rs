use crate::{
  comment::CommentResponse,
  community::CommunityResponse,
  context::LemmyContext,
  post::PostResponse,
  utils::{
    check_person_instance_community_block,
    get_interface_language,
    is_mod_or_admin,
    send_email_to_user,
  },
};
use actix_web::web::Json;
use lemmy_db_schema::{
  newtypes::{CommentId, CommunityId, LocalUserId, PostId},
  source::{
    actor_language::CommunityLanguage,
    comment::Comment,
    comment_reply::{CommentReply, CommentReplyInsertForm},
    community::Community,
    person::Person,
    person_mention::{PersonMention, PersonMentionInsertForm},
    post::Post,
  },
  traits::Crud,
};
use lemmy_db_views::structs::{CommentView, LocalUserView, PostView};
use lemmy_db_views_actor::structs::CommunityView;
use lemmy_utils::{
  error::LemmyResult,
  utils::{markdown::markdown_to_html, mention::MentionData},
};

pub async fn build_comment_response(
  context: &LemmyContext,
  comment_id: CommentId,
  local_user_view: Option<LocalUserView>,
  recipient_ids: Vec<LocalUserId>,
) -> LemmyResult<CommentResponse> {
  let local_user = local_user_view.map(|l| l.local_user);
  let comment_view =
    CommentView::read(&mut context.pool(), comment_id, local_user.as_ref()).await?;
  Ok(CommentResponse {
    comment_view,
    recipient_ids,
  })
}

pub async fn build_community_response(
  context: &LemmyContext,
  local_user_view: LocalUserView,
  community_id: CommunityId,
) -> LemmyResult<Json<CommunityResponse>> {
  let is_mod_or_admin = is_mod_or_admin(&mut context.pool(), &local_user_view.person, community_id)
    .await
    .is_ok();
  let local_user = local_user_view.local_user;
  let community_view = CommunityView::read(
    &mut context.pool(),
    community_id,
    Some(&local_user),
    is_mod_or_admin,
  )
  .await?;
  let discussion_languages = CommunityLanguage::read(&mut context.pool(), community_id).await?;

  Ok(Json(CommunityResponse {
    community_view,
    discussion_languages,
  }))
}

pub async fn build_post_response(
  context: &LemmyContext,
  community_id: CommunityId,
  local_user_view: LocalUserView,
  post_id: PostId,
) -> LemmyResult<Json<PostResponse>> {
  let local_user = local_user_view.local_user;
  let is_mod_or_admin = is_mod_or_admin(&mut context.pool(), &local_user_view.person, community_id)
    .await
    .is_ok();
  let post_view = PostView::read(
    &mut context.pool(),
    post_id,
    Some(&local_user),
    is_mod_or_admin,
  )
  .await?;
  Ok(Json(PostResponse { post_view }))
}

// TODO: this function is a mess and should be split up to handle email separately
#[tracing::instrument(skip_all)]
pub async fn send_local_notifs(
  mentions: Vec<MentionData>,
  comment_id: CommentId,
  person: &Person,
  do_send_email: bool,
  context: &LemmyContext,
  local_user_view: Option<&LocalUserView>,
) -> LemmyResult<Vec<LocalUserId>> {
  let mut recipient_ids = Vec::new();

  // When called from api code, we have local user view and can read with CommentView
  // to reduce db queries. But when receiving a federated comment the user view is None,
  // which means that comments inside private communities cant be read. As a workaround
  // we need to read the items manually to bypass this check.
  let (comment, post, community) = if let Some(local_user_view) = local_user_view {
    let comment_view = CommentView::read(
      &mut context.pool(),
      comment_id,
      Some(&local_user_view.local_user),
    )
    .await?;
    (
      comment_view.comment,
      comment_view.post,
      comment_view.community,
    )
  } else {
    let comment = Comment::read(&mut context.pool(), comment_id).await?;
    let post = Post::read(&mut context.pool(), comment.post_id).await?;
    let community = Community::read(&mut context.pool(), post.community_id).await?;
    (comment, post, community)
  };

  let inbox_link = format!("{}/inbox", context.settings().get_protocol_and_hostname());
  let comment_link = format!(
    "{}/post/{}/{}",
    context.settings().get_protocol_and_hostname(),
    post.id,
    comment_id
  );

  // Send the local mentions
  for mention in mentions
    .iter()
    .filter(|m| m.is_local(&context.settings().hostname) && m.name.ne(&person.name))
  {
    let mention_name = mention.name.clone();
    let user_view = LocalUserView::read_from_name(&mut context.pool(), &mention_name).await;
    if let Ok(mention_user_view) = user_view {
      // TODO
      // At some point, make it so you can't tag the parent creator either
      // Potential duplication of notifications, one for reply and the other for mention, is handled
      // below by checking recipient ids
      recipient_ids.push(mention_user_view.local_user.id);

      let user_mention_form = PersonMentionInsertForm {
        recipient_id: mention_user_view.person.id,
        comment_id,
        read: None,
      };

      // Allow this to fail softly, since comment edits might re-update or replace it
      // Let the uniqueness handle this fail
      PersonMention::create(&mut context.pool(), &user_mention_form)
        .await
        .ok();

      // Send an email to those local users that have notifications on
      if do_send_email {
        let lang = get_interface_language(&mention_user_view);
        let content = markdown_to_html(&comment.content);
        send_email_to_user(
          &mention_user_view,
          &lang.notification_mentioned_by_subject(&person.name),
          &lang.notification_mentioned_by_body(&comment_link, &content, &inbox_link, &person.name),
          context.settings(),
        )
        .await
      }
    }
  }

  // Send comment_reply to the parent commenter / poster
  if let Some(parent_comment_id) = comment.parent_comment_id() {
    let parent_comment = Comment::read(&mut context.pool(), parent_comment_id).await?;

    // Get the parent commenter local_user
    let parent_creator_id = parent_comment.creator_id;

    let check_blocks = check_person_instance_community_block(
      person.id,
      parent_creator_id,
      // Only block from the community's instance_id
      community.instance_id,
      community.id,
      &mut context.pool(),
    )
    .await
    .is_err();

    // Don't send a notif to yourself
    if parent_comment.creator_id != person.id && !check_blocks {
      let user_view = LocalUserView::read_person(&mut context.pool(), parent_creator_id).await;
      if let Ok(parent_user_view) = user_view {
        // Don't duplicate notif if already mentioned by checking recipient ids
        if !recipient_ids.contains(&parent_user_view.local_user.id) {
          recipient_ids.push(parent_user_view.local_user.id);

          let comment_reply_form = CommentReplyInsertForm {
            recipient_id: parent_user_view.person.id,
            comment_id: comment.id,
            read: None,
          };

          // Allow this to fail softly, since comment edits might re-update or replace it
          // Let the uniqueness handle this fail
          CommentReply::create(&mut context.pool(), &comment_reply_form)
            .await
            .ok();

          if do_send_email {
            let lang = get_interface_language(&parent_user_view);
            let content = markdown_to_html(&comment.content);
            send_email_to_user(
              &parent_user_view,
              &lang.notification_comment_reply_subject(&person.name),
              &lang.notification_comment_reply_body(
                &comment_link,
                &content,
                &inbox_link,
                &parent_comment.content,
                &post.name,
                &person.name,
              ),
              context.settings(),
            )
            .await
          }
        }
      }
    }
  } else {
    // Use the post creator to check blocks
    let check_blocks = check_person_instance_community_block(
      person.id,
      post.creator_id,
      // Only block from the community's instance_id
      community.instance_id,
      community.id,
      &mut context.pool(),
    )
    .await
    .is_err();

    if post.creator_id != person.id && !check_blocks {
      let creator_id = post.creator_id;
      let parent_user = LocalUserView::read_person(&mut context.pool(), creator_id).await;
      if let Ok(parent_user_view) = parent_user {
        if !recipient_ids.contains(&parent_user_view.local_user.id) {
          recipient_ids.push(parent_user_view.local_user.id);

          let comment_reply_form = CommentReplyInsertForm {
            recipient_id: parent_user_view.person.id,
            comment_id: comment.id,
            read: None,
          };

          // Allow this to fail softly, since comment edits might re-update or replace it
          // Let the uniqueness handle this fail
          CommentReply::create(&mut context.pool(), &comment_reply_form)
            .await
            .ok();

          if do_send_email {
            let lang = get_interface_language(&parent_user_view);
            let content = markdown_to_html(&comment.content);
            send_email_to_user(
              &parent_user_view,
              &lang.notification_post_reply_subject(&person.name),
              &lang.notification_post_reply_body(
                &comment_link,
                &content,
                &inbox_link,
                &post.name,
                &person.name,
              ),
              context.settings(),
            )
            .await
          }
        }
      }
    }
  }

  Ok(recipient_ids)
}
