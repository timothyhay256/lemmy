use crate::{
  activities::{
    check_community_deleted_or_removed,
    community::send_activity_in_community,
    generate_activity_id,
    generate_to,
    verify_person_in_community,
    verify_visibility,
  },
  activity_lists::AnnouncableActivities,
  insert_received_activity,
  objects::{community::ApubCommunity, person::ApubPerson, post::ApubPost},
  protocol::{
    activities::{create_or_update::page::CreateOrUpdatePage, CreateOrUpdateType},
    InCommunity,
  },
};
use activitypub_federation::{
  config::Data,
  protocol::verification::{verify_domains_match, verify_urls_match},
  traits::{ActivityHandler, Actor, Object},
};
use lemmy_api_common::context::LemmyContext;
use lemmy_db_schema::{
  aggregates::structs::PostAggregates,
  newtypes::PersonId,
  source::{
    activity::ActivitySendTargets,
    community::Community,
    person::Person,
    post::{Post, PostLike, PostLikeForm},
  },
  traits::{Crud, Likeable},
};
use lemmy_utils::error::{LemmyError, LemmyResult};
use url::Url;

impl CreateOrUpdatePage {
  pub(crate) async fn new(
    post: ApubPost,
    actor: &ApubPerson,
    community: &ApubCommunity,
    kind: CreateOrUpdateType,
    context: &Data<LemmyContext>,
  ) -> LemmyResult<CreateOrUpdatePage> {
    let id = generate_activity_id(
      kind.clone(),
      &context.settings().get_protocol_and_hostname(),
    )?;
    Ok(CreateOrUpdatePage {
      actor: actor.id().into(),
      to: generate_to(community)?,
      object: post.into_json(context).await?,
      cc: vec![community.id()],
      kind,
      id: id.clone(),
    })
  }

  #[tracing::instrument(skip_all)]
  pub(crate) async fn send(
    post: Post,
    person_id: PersonId,
    kind: CreateOrUpdateType,
    context: Data<LemmyContext>,
  ) -> LemmyResult<()> {
    let community_id = post.community_id;
    let person: ApubPerson = Person::read(&mut context.pool(), person_id).await?.into();
    let community: ApubCommunity = Community::read(&mut context.pool(), community_id)
      .await?
      .into();

    let create_or_update =
      CreateOrUpdatePage::new(post.into(), &person, &community, kind, &context).await?;
    let activity = AnnouncableActivities::CreateOrUpdatePost(create_or_update);
    send_activity_in_community(
      activity,
      &person,
      &community,
      ActivitySendTargets::empty(),
      false,
      &context,
    )
    .await?;
    Ok(())
  }
}

#[async_trait::async_trait]
impl ActivityHandler for CreateOrUpdatePage {
  type DataType = LemmyContext;
  type Error = LemmyError;

  fn id(&self) -> &Url {
    &self.id
  }

  fn actor(&self) -> &Url {
    self.actor.inner()
  }

  #[tracing::instrument(skip_all)]
  async fn verify(&self, context: &Data<LemmyContext>) -> LemmyResult<()> {
    let community = self.community(context).await?;
    verify_visibility(&self.to, &self.cc, &community)?;
    verify_person_in_community(&self.actor, &community, context).await?;
    check_community_deleted_or_removed(&community)?;
    verify_domains_match(self.actor.inner(), self.object.id.inner())?;
    verify_urls_match(self.actor.inner(), self.object.creator()?.inner())?;
    ApubPost::verify(&self.object, self.actor.inner(), context).await?;
    Ok(())
  }

  #[tracing::instrument(skip_all)]
  async fn receive(self, context: &Data<LemmyContext>) -> LemmyResult<()> {
    insert_received_activity(&self.id, context).await?;
    let post = ApubPost::from_json(self.object, context).await?;

    // author likes their own post by default
    let like_form = PostLikeForm::new(post.id, post.creator_id, 1);
    PostLike::like(&mut context.pool(), &like_form).await?;

    // Calculate initial hot_rank for post
    PostAggregates::update_ranks(&mut context.pool(), post.id).await?;

    Ok(())
  }
}
