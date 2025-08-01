use std::collections::{HashMap, HashSet};

use chrono::{NaiveDateTime, Utc};
use num_traits::ToPrimitive;
use rocket::fs::TempFile;
use rocket::serde::json::Json;
use rocket::{
    form::{Form, FromForm},
    Route,
};
use serde_json::Value;

use crate::auth::ClientVersion;
use crate::util::{save_temp_file, NumberOrString};
use crate::{
    api::{self, core::log_event, EmptyResult, JsonResult, Notify, PasswordOrOtpData, UpdateType},
    auth::Headers,
    config::PathType,
    crypto,
    db::{models::*, DbConn, DbPool},
    CONFIG,
};

use super::folders::FolderData;

pub fn routes() -> Vec<Route> {
    // Note that many routes have an `admin` variant; this seems to be
    // because the stored procedure that upstream Bitwarden uses to determine
    // whether the user can edit a cipher doesn't take into account whether
    // the user is an org owner/admin. The `admin` variant first checks
    // whether the user is an owner/admin of the relevant org, and if so,
    // allows the operation unconditionally.
    //
    // vaultwarden factors in the org owner/admin status as part of
    // determining the write accessibility of a cipher, so most
    // admin/non-admin implementations can be shared.
    routes![
        sync,
        get_ciphers,
        get_cipher,
        get_cipher_admin,
        get_cipher_details,
        post_ciphers,
        put_cipher_admin,
        post_ciphers_admin,
        post_ciphers_create,
        post_ciphers_import,
        get_attachment,
        post_attachment_v2,
        post_attachment_v2_data,
        post_attachment,       // legacy
        post_attachment_admin, // legacy
        post_attachment_share,
        delete_attachment_post,
        delete_attachment_post_admin,
        delete_attachment,
        delete_attachment_admin,
        post_cipher_admin,
        post_cipher_share,
        put_cipher_share,
        put_cipher_share_selected,
        post_cipher,
        post_cipher_partial,
        put_cipher,
        put_cipher_partial,
        delete_cipher_post,
        delete_cipher_post_admin,
        delete_cipher_put,
        delete_cipher_put_admin,
        delete_cipher,
        delete_cipher_admin,
        delete_cipher_selected,
        delete_cipher_selected_post,
        delete_cipher_selected_put,
        delete_cipher_selected_admin,
        delete_cipher_selected_post_admin,
        delete_cipher_selected_put_admin,
        restore_cipher_put,
        restore_cipher_put_admin,
        restore_cipher_selected,
        delete_all,
        move_cipher_selected,
        move_cipher_selected_put,
        put_collections2_update,
        post_collections2_update,
        put_collections_update,
        post_collections_update,
        post_collections_admin,
        put_collections_admin,
    ]
}

pub async fn purge_trashed_ciphers(pool: DbPool) {
    debug!("Purging trashed ciphers");
    if let Ok(mut conn) = pool.get().await {
        Cipher::purge_trash(&mut conn).await;
    } else {
        error!("Failed to get DB connection while purging trashed ciphers")
    }
}

#[derive(FromForm, Default)]
struct SyncData {
    #[field(name = "excludeDomains")]
    exclude_domains: bool, // Default: 'false'
}

#[get("/sync?<data..>")]
async fn sync(data: SyncData, headers: Headers, client_version: Option<ClientVersion>, mut conn: DbConn) -> JsonResult {
    let user_json = headers.user.to_json(&mut conn).await;

    // Get all ciphers which are visible by the user
    let mut ciphers = Cipher::find_by_user_visible(&headers.user.uuid, &mut conn).await;

    // Filter out SSH keys if the client version is less than 2024.12.0
    let show_ssh_keys = if let Some(client_version) = client_version {
        let ver_match = semver::VersionReq::parse(">=2024.12.0").unwrap();
        ver_match.matches(&client_version.0)
    } else {
        false
    };
    if !show_ssh_keys {
        ciphers.retain(|c| c.atype != 5);
    }

    let cipher_sync_data = CipherSyncData::new(&headers.user.uuid, CipherSyncType::User, &mut conn).await;

    // Lets generate the ciphers_json using all the gathered info
    let mut ciphers_json = Vec::with_capacity(ciphers.len());
    for c in ciphers {
        ciphers_json.push(
            c.to_json(&headers.host, &headers.user.uuid, Some(&cipher_sync_data), CipherSyncType::User, &mut conn)
                .await?,
        );
    }

    let collections = Collection::find_by_user_uuid(headers.user.uuid.clone(), &mut conn).await;
    let mut collections_json = Vec::with_capacity(collections.len());
    for c in collections {
        collections_json.push(c.to_json_details(&headers.user.uuid, Some(&cipher_sync_data), &mut conn).await);
    }

    let folders_json: Vec<Value> =
        Folder::find_by_user(&headers.user.uuid, &mut conn).await.iter().map(Folder::to_json).collect();

    let sends_json: Vec<Value> =
        Send::find_by_user(&headers.user.uuid, &mut conn).await.iter().map(Send::to_json).collect();

    let policies_json: Vec<Value> =
        OrgPolicy::find_confirmed_by_user(&headers.user.uuid, &mut conn).await.iter().map(OrgPolicy::to_json).collect();

    let domains_json = if data.exclude_domains {
        Value::Null
    } else {
        api::core::_get_eq_domains(headers, true).into_inner()
    };

    Ok(Json(json!({
        "profile": user_json,
        "folders": folders_json,
        "collections": collections_json,
        "policies": policies_json,
        "ciphers": ciphers_json,
        "domains": domains_json,
        "sends": sends_json,
        "object": "sync"
    })))
}

#[get("/ciphers")]
async fn get_ciphers(headers: Headers, mut conn: DbConn) -> JsonResult {
    let ciphers = Cipher::find_by_user_visible(&headers.user.uuid, &mut conn).await;
    let cipher_sync_data = CipherSyncData::new(&headers.user.uuid, CipherSyncType::User, &mut conn).await;

    let mut ciphers_json = Vec::with_capacity(ciphers.len());
    for c in ciphers {
        ciphers_json.push(
            c.to_json(&headers.host, &headers.user.uuid, Some(&cipher_sync_data), CipherSyncType::User, &mut conn)
                .await?,
        );
    }

    Ok(Json(json!({
      "data": ciphers_json,
      "object": "list",
      "continuationToken": null
    })))
}

#[get("/ciphers/<cipher_id>")]
async fn get_cipher(cipher_id: CipherId, headers: Headers, mut conn: DbConn) -> JsonResult {
    let Some(cipher) = Cipher::find_by_uuid(&cipher_id, &mut conn).await else {
        err!("Cipher doesn't exist")
    };

    if !cipher.is_accessible_to_user(&headers.user.uuid, &mut conn).await {
        err!("Cipher is not owned by user")
    }

    Ok(Json(cipher.to_json(&headers.host, &headers.user.uuid, None, CipherSyncType::User, &mut conn).await?))
}

#[get("/ciphers/<cipher_id>/admin")]
async fn get_cipher_admin(cipher_id: CipherId, headers: Headers, conn: DbConn) -> JsonResult {
    // TODO: Implement this correctly
    get_cipher(cipher_id, headers, conn).await
}

#[get("/ciphers/<cipher_id>/details")]
async fn get_cipher_details(cipher_id: CipherId, headers: Headers, conn: DbConn) -> JsonResult {
    get_cipher(cipher_id, headers, conn).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CipherData {
    // Id is optional as it is included only in bulk share
    pub id: Option<CipherId>,
    // Folder id is not included in import
    pub folder_id: Option<FolderId>,
    // TODO: Some of these might appear all the time, no need for Option
    #[serde(alias = "organizationID")]
    pub organization_id: Option<OrganizationId>,

    key: Option<String>,

    /*
    Login = 1,
    SecureNote = 2,
    Card = 3,
    Identity = 4,
    SshKey = 5
    */
    pub r#type: i32,
    pub name: String,
    pub notes: Option<String>,
    fields: Option<Value>,

    // Only one of these should exist, depending on type
    login: Option<Value>,
    secure_note: Option<Value>,
    card: Option<Value>,
    identity: Option<Value>,
    ssh_key: Option<Value>,

    favorite: Option<bool>,
    reprompt: Option<i32>,

    pub password_history: Option<Value>,

    // These are used during key rotation
    // 'Attachments' is unused, contains map of {id: filename}
    #[allow(dead_code)]
    attachments: Option<Value>,
    attachments2: Option<HashMap<AttachmentId, Attachments2Data>>,

    // The revision datetime (in ISO 8601 format) of the client's local copy
    // of the cipher. This is used to prevent a client from updating a cipher
    // when it doesn't have the latest version, as that can result in data
    // loss. It's not an error when no value is provided; this can happen
    // when using older client versions, or if the operation doesn't involve
    // updating an existing cipher.
    last_known_revision_date: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialCipherData {
    folder_id: Option<FolderId>,
    favorite: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachments2Data {
    file_name: String,
    key: String,
}

/// Called when an org admin clones an org cipher.
#[post("/ciphers/admin", data = "<data>")]
async fn post_ciphers_admin(data: Json<ShareCipherData>, headers: Headers, conn: DbConn, nt: Notify<'_>) -> JsonResult {
    post_ciphers_create(data, headers, conn, nt).await
}

/// Called when creating a new org-owned cipher, or cloning a cipher (whether
/// user- or org-owned). When cloning a cipher to a user-owned cipher,
/// `organizationId` is null.
#[post("/ciphers/create", data = "<data>")]
async fn post_ciphers_create(
    data: Json<ShareCipherData>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    let mut data: ShareCipherData = data.into_inner();

    // Check if there are one more more collections selected when this cipher is part of an organization.
    // err if this is not the case before creating an empty cipher.
    if data.cipher.organization_id.is_some() && data.collection_ids.is_empty() {
        err!("You must select at least one collection.");
    }

    // This check is usually only needed in update_cipher_from_data(), but we
    // need it here as well to avoid creating an empty cipher in the call to
    // cipher.save() below.
    enforce_personal_ownership_policy(Some(&data.cipher), &headers, &mut conn).await?;

    let mut cipher = Cipher::new(data.cipher.r#type, data.cipher.name.clone());
    cipher.user_uuid = Some(headers.user.uuid.clone());
    cipher.save(&mut conn).await?;

    // When cloning a cipher, the Bitwarden clients seem to set this field
    // based on the cipher being cloned (when creating a new cipher, it's set
    // to null as expected). However, `cipher.created_at` is initialized to
    // the current time, so the stale data check will end up failing down the
    // line. Since this function only creates new ciphers (whether by cloning
    // or otherwise), we can just ignore this field entirely.
    data.cipher.last_known_revision_date = None;

    share_cipher_by_uuid(&cipher.uuid, data, &headers, &mut conn, &nt).await
}

/// Called when creating a new user-owned cipher.
#[post("/ciphers", data = "<data>")]
async fn post_ciphers(data: Json<CipherData>, headers: Headers, mut conn: DbConn, nt: Notify<'_>) -> JsonResult {
    let mut data: CipherData = data.into_inner();

    // The web/browser clients set this field to null as expected, but the
    // mobile clients seem to set the invalid value `0001-01-01T00:00:00`,
    // which results in a warning message being logged. This field isn't
    // needed when creating a new cipher, so just ignore it unconditionally.
    data.last_known_revision_date = None;

    let mut cipher = Cipher::new(data.r#type, data.name.clone());
    update_cipher_from_data(&mut cipher, data, &headers, None, &mut conn, &nt, UpdateType::SyncCipherCreate).await?;

    Ok(Json(cipher.to_json(&headers.host, &headers.user.uuid, None, CipherSyncType::User, &mut conn).await?))
}

/// Enforces the personal ownership policy on user-owned ciphers, if applicable.
/// A non-owner/admin user belonging to an org with the personal ownership policy
/// enabled isn't allowed to create new user-owned ciphers or modify existing ones
/// (that were created before the policy was applicable to the user). The user is
/// allowed to delete or share such ciphers to an org, however.
///
/// Ref: https://bitwarden.com/help/article/policies/#personal-ownership
async fn enforce_personal_ownership_policy(
    data: Option<&CipherData>,
    headers: &Headers,
    conn: &mut DbConn,
) -> EmptyResult {
    if data.is_none() || data.unwrap().organization_id.is_none() {
        let user_id = &headers.user.uuid;
        let policy_type = OrgPolicyType::PersonalOwnership;
        if OrgPolicy::is_applicable_to_user(user_id, policy_type, None, conn).await {
            err!("Due to an Enterprise Policy, you are restricted from saving items to your personal vault.")
        }
    }
    Ok(())
}

pub async fn update_cipher_from_data(
    cipher: &mut Cipher,
    data: CipherData,
    headers: &Headers,
    shared_to_collections: Option<Vec<CollectionId>>,
    conn: &mut DbConn,
    nt: &Notify<'_>,
    ut: UpdateType,
) -> EmptyResult {
    enforce_personal_ownership_policy(Some(&data), headers, conn).await?;

    // Check that the client isn't updating an existing cipher with stale data.
    // And only perform this check when not importing ciphers, else the date/time check will fail.
    if ut != UpdateType::None {
        if let Some(dt) = data.last_known_revision_date {
            match NaiveDateTime::parse_from_str(&dt, "%+") {
                // ISO 8601 format
                Err(err) => warn!("Error parsing LastKnownRevisionDate '{dt}': {err}"),
                Ok(dt) if cipher.updated_at.signed_duration_since(dt).num_seconds() > 1 => {
                    err!("The client copy of this cipher is out of date. Resync the client and try again.")
                }
                Ok(_) => (),
            }
        }
    }

    if cipher.organization_uuid.is_some() && cipher.organization_uuid != data.organization_id {
        err!("Organization mismatch. Please resync the client before updating the cipher")
    }

    if let Some(note) = &data.notes {
        let max_note_size = CONFIG._max_note_size();
        if note.len() > max_note_size {
            err!(format!("The field Notes exceeds the maximum encrypted value length of {max_note_size} characters."))
        }
    }

    // Check if this cipher is being transferred from a personal to an organization vault
    let transfer_cipher = cipher.organization_uuid.is_none() && data.organization_id.is_some();

    if let Some(org_id) = data.organization_id {
        match Membership::find_by_user_and_org(&headers.user.uuid, &org_id, conn).await {
            None => err!("You don't have permission to add item to organization"),
            Some(member) => {
                if shared_to_collections.is_some()
                    || member.has_full_access()
                    || cipher.is_write_accessible_to_user(&headers.user.uuid, conn).await
                {
                    cipher.organization_uuid = Some(org_id);
                    // After some discussion in PR #1329 re-added the user_uuid = None again.
                    // TODO: Audit/Check the whole save/update cipher chain.
                    // Upstream uses the user_uuid to allow a cipher added by a user to an org to still allow the user to view/edit the cipher
                    // even when the user has hide-passwords configured as there policy.
                    // Removing the line below would fix that, but we have to check which effect this would have on the rest of the code.
                    cipher.user_uuid = None;
                } else {
                    err!("You don't have permission to add cipher directly to organization")
                }
            }
        }
    } else {
        cipher.user_uuid = Some(headers.user.uuid.clone());
    }

    if let Some(ref folder_id) = data.folder_id {
        if Folder::find_by_uuid_and_user(folder_id, &headers.user.uuid, conn).await.is_none() {
            err!("Invalid folder", "Folder does not exist or belongs to another user");
        }
    }

    // Modify attachments name and keys when rotating
    if let Some(attachments) = data.attachments2 {
        for (id, attachment) in attachments {
            let mut saved_att = match Attachment::find_by_id(&id, conn).await {
                Some(att) => att,
                None => {
                    // Warn and continue here.
                    // A missing attachment means it was removed via an other client.
                    // Also the Desktop Client supports removing attachments and save an update afterwards.
                    // Bitwarden it self ignores these mismatches server side.
                    warn!("Attachment {id} doesn't exist");
                    continue;
                }
            };

            if saved_att.cipher_uuid != cipher.uuid {
                // Warn and break here since cloning ciphers provides attachment data but will not be cloned.
                // If we error out here it will break the whole cloning and causes empty ciphers to appear.
                warn!("Attachment is not owned by the cipher");
                break;
            }

            saved_att.akey = Some(attachment.key);
            saved_att.file_name = attachment.file_name;

            saved_att.save(conn).await?;
        }
    }

    // Cleanup cipher data, like removing the 'Response' key.
    // This key is somewhere generated during Javascript so no way for us this fix this.
    // Also, upstream only retrieves keys they actually want to store, and thus skip the 'Response' key.
    // We do not mind which data is in it, the keep our model more flexible when there are upstream changes.
    // But, we at least know we do not need to store and return this specific key.
    fn _clean_cipher_data(mut json_data: Value) -> Value {
        if json_data.is_array() {
            json_data.as_array_mut().unwrap().iter_mut().for_each(|ref mut f| {
                f.as_object_mut().unwrap().remove("response");
            });
        };
        json_data
    }

    let type_data_opt = match data.r#type {
        1 => data.login,
        2 => data.secure_note,
        3 => data.card,
        4 => data.identity,
        5 => data.ssh_key,
        _ => err!("Invalid type"),
    };

    let type_data = match type_data_opt {
        Some(mut data) => {
            // Remove the 'Response' key from the base object.
            data.as_object_mut().unwrap().remove("response");
            // Remove the 'Response' key from every Uri.
            if data["uris"].is_array() {
                data["uris"] = _clean_cipher_data(data["uris"].clone());
            }
            data
        }
        None => err!("Data missing"),
    };

    cipher.key = data.key;
    cipher.name = data.name;
    cipher.notes = data.notes;
    cipher.fields = data.fields.map(|f| _clean_cipher_data(f).to_string());
    cipher.data = type_data.to_string();
    cipher.password_history = data.password_history.map(|f| f.to_string());
    cipher.reprompt = data.reprompt.filter(|r| *r == RepromptType::None as i32 || *r == RepromptType::Password as i32);

    cipher.save(conn).await?;
    cipher.move_to_folder(data.folder_id, &headers.user.uuid, conn).await?;
    cipher.set_favorite(data.favorite, &headers.user.uuid, conn).await?;

    if ut != UpdateType::None {
        // Only log events for organizational ciphers
        if let Some(org_id) = &cipher.organization_uuid {
            let event_type = match (&ut, transfer_cipher) {
                (UpdateType::SyncCipherCreate, true) => EventType::CipherCreated,
                (UpdateType::SyncCipherUpdate, true) => EventType::CipherShared,
                (_, _) => EventType::CipherUpdated,
            };

            log_event(
                event_type as i32,
                &cipher.uuid,
                org_id,
                &headers.user.uuid,
                headers.device.atype,
                &headers.ip.ip,
                conn,
            )
            .await;
        }
        nt.send_cipher_update(
            ut,
            cipher,
            &cipher.update_users_revision(conn).await,
            &headers.device,
            shared_to_collections,
            conn,
        )
        .await;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportData {
    ciphers: Vec<CipherData>,
    folders: Vec<FolderData>,
    folder_relationships: Vec<RelationsData>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelationsData {
    // Cipher id
    key: usize,
    // Folder id
    value: usize,
}

#[post("/ciphers/import", data = "<data>")]
async fn post_ciphers_import(
    data: Json<ImportData>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    enforce_personal_ownership_policy(None, &headers, &mut conn).await?;

    let data: ImportData = data.into_inner();

    // Validate the import before continuing
    // Bitwarden does not process the import if there is one item invalid.
    // Since we check for the size of the encrypted note length, we need to do that here to pre-validate it.
    // TODO: See if we can optimize the whole cipher adding/importing and prevent duplicate code and checks.
    Cipher::validate_cipher_data(&data.ciphers)?;

    // Read and create the folders
    let existing_folders: HashSet<Option<FolderId>> =
        Folder::find_by_user(&headers.user.uuid, &mut conn).await.into_iter().map(|f| Some(f.uuid)).collect();
    let mut folders: Vec<FolderId> = Vec::with_capacity(data.folders.len());
    for folder in data.folders.into_iter() {
        let folder_id = if existing_folders.contains(&folder.id) {
            folder.id.unwrap()
        } else {
            let mut new_folder = Folder::new(headers.user.uuid.clone(), folder.name);
            new_folder.save(&mut conn).await?;
            new_folder.uuid
        };

        folders.push(folder_id);
    }

    // Read the relations between folders and ciphers
    // Ciphers can only be in one folder at the same time
    let mut relations_map = HashMap::with_capacity(data.folder_relationships.len());
    for relation in data.folder_relationships {
        relations_map.insert(relation.key, relation.value);
    }

    // Read and create the ciphers
    for (index, mut cipher_data) in data.ciphers.into_iter().enumerate() {
        let folder_id = relations_map.get(&index).map(|i| folders[*i].clone());
        cipher_data.folder_id = folder_id;

        let mut cipher = Cipher::new(cipher_data.r#type, cipher_data.name.clone());
        update_cipher_from_data(&mut cipher, cipher_data, &headers, None, &mut conn, &nt, UpdateType::None).await?;
    }

    let mut user = headers.user;
    user.update_revision(&mut conn).await?;
    nt.send_user_update(UpdateType::SyncVault, &user, &headers.device.push_uuid, &mut conn).await;

    Ok(())
}

/// Called when an org admin modifies an existing org cipher.
#[put("/ciphers/<cipher_id>/admin", data = "<data>")]
async fn put_cipher_admin(
    cipher_id: CipherId,
    data: Json<CipherData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    put_cipher(cipher_id, data, headers, conn, nt).await
}

#[post("/ciphers/<cipher_id>/admin", data = "<data>")]
async fn post_cipher_admin(
    cipher_id: CipherId,
    data: Json<CipherData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    post_cipher(cipher_id, data, headers, conn, nt).await
}

#[post("/ciphers/<cipher_id>", data = "<data>")]
async fn post_cipher(
    cipher_id: CipherId,
    data: Json<CipherData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    put_cipher(cipher_id, data, headers, conn, nt).await
}

#[put("/ciphers/<cipher_id>", data = "<data>")]
async fn put_cipher(
    cipher_id: CipherId,
    data: Json<CipherData>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    let data: CipherData = data.into_inner();

    let Some(mut cipher) = Cipher::find_by_uuid(&cipher_id, &mut conn).await else {
        err!("Cipher doesn't exist")
    };

    // TODO: Check if only the folder ID or favorite status is being changed.
    // These are per-user properties that technically aren't part of the
    // cipher itself, so the user shouldn't need write access to change these.
    // Interestingly, upstream Bitwarden doesn't properly handle this either.

    if !cipher.is_write_accessible_to_user(&headers.user.uuid, &mut conn).await {
        err!("Cipher is not write accessible")
    }

    update_cipher_from_data(&mut cipher, data, &headers, None, &mut conn, &nt, UpdateType::SyncCipherUpdate).await?;

    Ok(Json(cipher.to_json(&headers.host, &headers.user.uuid, None, CipherSyncType::User, &mut conn).await?))
}

#[post("/ciphers/<cipher_id>/partial", data = "<data>")]
async fn post_cipher_partial(
    cipher_id: CipherId,
    data: Json<PartialCipherData>,
    headers: Headers,
    conn: DbConn,
) -> JsonResult {
    put_cipher_partial(cipher_id, data, headers, conn).await
}

// Only update the folder and favorite for the user, since this cipher is read-only
#[put("/ciphers/<cipher_id>/partial", data = "<data>")]
async fn put_cipher_partial(
    cipher_id: CipherId,
    data: Json<PartialCipherData>,
    headers: Headers,
    mut conn: DbConn,
) -> JsonResult {
    let data: PartialCipherData = data.into_inner();

    let Some(cipher) = Cipher::find_by_uuid(&cipher_id, &mut conn).await else {
        err!("Cipher doesn't exist")
    };

    if let Some(ref folder_id) = data.folder_id {
        if Folder::find_by_uuid_and_user(folder_id, &headers.user.uuid, &mut conn).await.is_none() {
            err!("Invalid folder", "Folder does not exist or belongs to another user");
        }
    }

    // Move cipher
    cipher.move_to_folder(data.folder_id.clone(), &headers.user.uuid, &mut conn).await?;
    // Update favorite
    cipher.set_favorite(Some(data.favorite), &headers.user.uuid, &mut conn).await?;

    Ok(Json(cipher.to_json(&headers.host, &headers.user.uuid, None, CipherSyncType::User, &mut conn).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionsAdminData {
    #[serde(alias = "CollectionIds")]
    collection_ids: Vec<CollectionId>,
}

#[put("/ciphers/<cipher_id>/collections_v2", data = "<data>")]
async fn put_collections2_update(
    cipher_id: CipherId,
    data: Json<CollectionsAdminData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    post_collections2_update(cipher_id, data, headers, conn, nt).await
}

#[post("/ciphers/<cipher_id>/collections_v2", data = "<data>")]
async fn post_collections2_update(
    cipher_id: CipherId,
    data: Json<CollectionsAdminData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    let cipher_details = post_collections_update(cipher_id, data, headers, conn, nt).await?;
    Ok(Json(json!({ // AttachmentUploadDataResponseModel
        "object": "optionalCipherDetails",
        "unavailable": false,
        "cipher": *cipher_details
    })))
}

#[put("/ciphers/<cipher_id>/collections", data = "<data>")]
async fn put_collections_update(
    cipher_id: CipherId,
    data: Json<CollectionsAdminData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    post_collections_update(cipher_id, data, headers, conn, nt).await
}

#[post("/ciphers/<cipher_id>/collections", data = "<data>")]
async fn post_collections_update(
    cipher_id: CipherId,
    data: Json<CollectionsAdminData>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    let data: CollectionsAdminData = data.into_inner();

    let Some(cipher) = Cipher::find_by_uuid(&cipher_id, &mut conn).await else {
        err!("Cipher doesn't exist")
    };

    if !cipher.is_write_accessible_to_user(&headers.user.uuid, &mut conn).await {
        err!("Cipher is not write accessible")
    }

    let posted_collections = HashSet::<CollectionId>::from_iter(data.collection_ids);
    let current_collections =
        HashSet::<CollectionId>::from_iter(cipher.get_collections(headers.user.uuid.clone(), &mut conn).await);

    for collection in posted_collections.symmetric_difference(&current_collections) {
        match Collection::find_by_uuid_and_org(collection, cipher.organization_uuid.as_ref().unwrap(), &mut conn).await
        {
            None => err!("Invalid collection ID provided"),
            Some(collection) => {
                if collection.is_writable_by_user(&headers.user.uuid, &mut conn).await {
                    if posted_collections.contains(&collection.uuid) {
                        // Add to collection
                        CollectionCipher::save(&cipher.uuid, &collection.uuid, &mut conn).await?;
                    } else {
                        // Remove from collection
                        CollectionCipher::delete(&cipher.uuid, &collection.uuid, &mut conn).await?;
                    }
                } else {
                    err!("No rights to modify the collection")
                }
            }
        }
    }

    nt.send_cipher_update(
        UpdateType::SyncCipherUpdate,
        &cipher,
        &cipher.update_users_revision(&mut conn).await,
        &headers.device,
        Some(Vec::from_iter(posted_collections)),
        &mut conn,
    )
    .await;

    log_event(
        EventType::CipherUpdatedCollections as i32,
        &cipher.uuid,
        &cipher.organization_uuid.clone().unwrap(),
        &headers.user.uuid,
        headers.device.atype,
        &headers.ip.ip,
        &mut conn,
    )
    .await;

    Ok(Json(cipher.to_json(&headers.host, &headers.user.uuid, None, CipherSyncType::User, &mut conn).await?))
}

#[put("/ciphers/<cipher_id>/collections-admin", data = "<data>")]
async fn put_collections_admin(
    cipher_id: CipherId,
    data: Json<CollectionsAdminData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    post_collections_admin(cipher_id, data, headers, conn, nt).await
}

#[post("/ciphers/<cipher_id>/collections-admin", data = "<data>")]
async fn post_collections_admin(
    cipher_id: CipherId,
    data: Json<CollectionsAdminData>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    let data: CollectionsAdminData = data.into_inner();

    let Some(cipher) = Cipher::find_by_uuid(&cipher_id, &mut conn).await else {
        err!("Cipher doesn't exist")
    };

    if !cipher.is_write_accessible_to_user(&headers.user.uuid, &mut conn).await {
        err!("Cipher is not write accessible")
    }

    let posted_collections = HashSet::<CollectionId>::from_iter(data.collection_ids);
    let current_collections =
        HashSet::<CollectionId>::from_iter(cipher.get_admin_collections(headers.user.uuid.clone(), &mut conn).await);

    for collection in posted_collections.symmetric_difference(&current_collections) {
        match Collection::find_by_uuid_and_org(collection, cipher.organization_uuid.as_ref().unwrap(), &mut conn).await
        {
            None => err!("Invalid collection ID provided"),
            Some(collection) => {
                if collection.is_writable_by_user(&headers.user.uuid, &mut conn).await {
                    if posted_collections.contains(&collection.uuid) {
                        // Add to collection
                        CollectionCipher::save(&cipher.uuid, &collection.uuid, &mut conn).await?;
                    } else {
                        // Remove from collection
                        CollectionCipher::delete(&cipher.uuid, &collection.uuid, &mut conn).await?;
                    }
                } else {
                    err!("No rights to modify the collection")
                }
            }
        }
    }

    nt.send_cipher_update(
        UpdateType::SyncCipherUpdate,
        &cipher,
        &cipher.update_users_revision(&mut conn).await,
        &headers.device,
        Some(Vec::from_iter(posted_collections)),
        &mut conn,
    )
    .await;

    log_event(
        EventType::CipherUpdatedCollections as i32,
        &cipher.uuid,
        &cipher.organization_uuid.unwrap(),
        &headers.user.uuid,
        headers.device.atype,
        &headers.ip.ip,
        &mut conn,
    )
    .await;

    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShareCipherData {
    #[serde(alias = "Cipher")]
    cipher: CipherData,
    #[serde(alias = "CollectionIds")]
    collection_ids: Vec<CollectionId>,
}

#[post("/ciphers/<cipher_id>/share", data = "<data>")]
async fn post_cipher_share(
    cipher_id: CipherId,
    data: Json<ShareCipherData>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    let data: ShareCipherData = data.into_inner();

    share_cipher_by_uuid(&cipher_id, data, &headers, &mut conn, &nt).await
}

#[put("/ciphers/<cipher_id>/share", data = "<data>")]
async fn put_cipher_share(
    cipher_id: CipherId,
    data: Json<ShareCipherData>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    let data: ShareCipherData = data.into_inner();

    share_cipher_by_uuid(&cipher_id, data, &headers, &mut conn, &nt).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShareSelectedCipherData {
    ciphers: Vec<CipherData>,
    collection_ids: Vec<CollectionId>,
}

#[put("/ciphers/share", data = "<data>")]
async fn put_cipher_share_selected(
    data: Json<ShareSelectedCipherData>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    let mut data: ShareSelectedCipherData = data.into_inner();

    if data.ciphers.is_empty() {
        err!("You must select at least one cipher.")
    }

    if data.collection_ids.is_empty() {
        err!("You must select at least one collection.")
    }

    for cipher in data.ciphers.iter() {
        if cipher.id.is_none() {
            err!("Request missing ids field")
        }
    }

    while let Some(cipher) = data.ciphers.pop() {
        let mut shared_cipher_data = ShareCipherData {
            cipher,
            collection_ids: data.collection_ids.clone(),
        };

        match shared_cipher_data.cipher.id.take() {
            Some(id) => share_cipher_by_uuid(&id, shared_cipher_data, &headers, &mut conn, &nt).await?,
            None => err!("Request missing ids field"),
        };
    }

    Ok(())
}

async fn share_cipher_by_uuid(
    cipher_id: &CipherId,
    data: ShareCipherData,
    headers: &Headers,
    conn: &mut DbConn,
    nt: &Notify<'_>,
) -> JsonResult {
    let mut cipher = match Cipher::find_by_uuid(cipher_id, conn).await {
        Some(cipher) => {
            if cipher.is_write_accessible_to_user(&headers.user.uuid, conn).await {
                cipher
            } else {
                err!("Cipher is not write accessible")
            }
        }
        None => err!("Cipher doesn't exist"),
    };

    let mut shared_to_collections = vec![];

    if let Some(organization_id) = &data.cipher.organization_id {
        for col_id in &data.collection_ids {
            match Collection::find_by_uuid_and_org(col_id, organization_id, conn).await {
                None => err!("Invalid collection ID provided"),
                Some(collection) => {
                    if collection.is_writable_by_user(&headers.user.uuid, conn).await {
                        CollectionCipher::save(&cipher.uuid, &collection.uuid, conn).await?;
                        shared_to_collections.push(collection.uuid);
                    } else {
                        err!("No rights to modify the collection")
                    }
                }
            }
        }
    };

    // When LastKnownRevisionDate is None, it is a new cipher, so send CipherCreate.
    let ut = if data.cipher.last_known_revision_date.is_some() {
        UpdateType::SyncCipherUpdate
    } else {
        UpdateType::SyncCipherCreate
    };

    update_cipher_from_data(&mut cipher, data.cipher, headers, Some(shared_to_collections), conn, nt, ut).await?;

    Ok(Json(cipher.to_json(&headers.host, &headers.user.uuid, None, CipherSyncType::User, conn).await?))
}

/// v2 API for downloading an attachment. This just redirects the client to
/// the actual location of an attachment.
///
/// Upstream added this v2 API to support direct download of attachments from
/// their object storage service. For self-hosted instances, it basically just
/// redirects to the same location as before the v2 API.
#[get("/ciphers/<cipher_id>/attachment/<attachment_id>")]
async fn get_attachment(
    cipher_id: CipherId,
    attachment_id: AttachmentId,
    headers: Headers,
    mut conn: DbConn,
) -> JsonResult {
    let Some(cipher) = Cipher::find_by_uuid(&cipher_id, &mut conn).await else {
        err!("Cipher doesn't exist")
    };

    if !cipher.is_accessible_to_user(&headers.user.uuid, &mut conn).await {
        err!("Cipher is not accessible")
    }

    match Attachment::find_by_id(&attachment_id, &mut conn).await {
        Some(attachment) if cipher_id == attachment.cipher_uuid => Ok(Json(attachment.to_json(&headers.host).await?)),
        Some(_) => err!("Attachment doesn't belong to cipher"),
        None => err!("Attachment doesn't exist"),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentRequestData {
    key: String,
    file_name: String,
    file_size: NumberOrString,
    admin_request: Option<bool>, // true when attaching from an org vault view
}

enum FileUploadType {
    Direct = 0,
    // Azure = 1, // only used upstream
}

/// v2 API for creating an attachment associated with a cipher.
/// This redirects the client to the API it should use to upload the attachment.
/// For upstream's cloud-hosted service, it's an Azure object storage API.
/// For self-hosted instances, it's another API on the local instance.
#[post("/ciphers/<cipher_id>/attachment/v2", data = "<data>")]
async fn post_attachment_v2(
    cipher_id: CipherId,
    data: Json<AttachmentRequestData>,
    headers: Headers,
    mut conn: DbConn,
) -> JsonResult {
    let Some(cipher) = Cipher::find_by_uuid(&cipher_id, &mut conn).await else {
        err!("Cipher doesn't exist")
    };

    if !cipher.is_write_accessible_to_user(&headers.user.uuid, &mut conn).await {
        err!("Cipher is not write accessible")
    }

    let data: AttachmentRequestData = data.into_inner();
    let file_size = data.file_size.into_i64()?;

    if file_size < 0 {
        err!("Attachment size can't be negative")
    }
    let attachment_id = crypto::generate_attachment_id();
    let attachment =
        Attachment::new(attachment_id.clone(), cipher.uuid.clone(), data.file_name, file_size, Some(data.key));
    attachment.save(&mut conn).await.expect("Error saving attachment");

    let url = format!("/ciphers/{}/attachment/{attachment_id}", cipher.uuid);
    let response_key = match data.admin_request {
        Some(b) if b => "cipherMiniResponse",
        _ => "cipherResponse",
    };

    Ok(Json(json!({ // AttachmentUploadDataResponseModel
        "object": "attachment-fileUpload",
        "attachmentId": attachment_id,
        "url": url,
        "fileUploadType": FileUploadType::Direct as i32,
        response_key: cipher.to_json(&headers.host, &headers.user.uuid, None, CipherSyncType::User, &mut conn).await?,
    })))
}

#[derive(FromForm)]
struct UploadData<'f> {
    key: Option<String>,
    data: TempFile<'f>,
}

/// Saves the data content of an attachment to a file. This is common code
/// shared between the v2 and legacy attachment APIs.
///
/// When used with the legacy API, this function is responsible for creating
/// the attachment database record, so `attachment` is None.
///
/// When used with the v2 API, post_attachment_v2() has already created the
/// database record, which is passed in as `attachment`.
async fn save_attachment(
    mut attachment: Option<Attachment>,
    cipher_id: CipherId,
    data: Form<UploadData<'_>>,
    headers: &Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> Result<(Cipher, DbConn), crate::error::Error> {
    let data = data.into_inner();

    let Some(size) = data.data.len().to_i64() else {
        err!("Attachment data size overflow");
    };
    if size < 0 {
        err!("Attachment size can't be negative")
    }

    let Some(cipher) = Cipher::find_by_uuid(&cipher_id, &mut conn).await else {
        err!("Cipher doesn't exist")
    };

    if !cipher.is_write_accessible_to_user(&headers.user.uuid, &mut conn).await {
        err!("Cipher is not write accessible")
    }

    // In the v2 API, the attachment record has already been created,
    // so the size limit needs to be adjusted to account for that.
    let size_adjust = match &attachment {
        None => 0,              // Legacy API
        Some(a) => a.file_size, // v2 API
    };

    let size_limit = if let Some(ref user_id) = cipher.user_uuid {
        match CONFIG.user_attachment_limit() {
            Some(0) => err!("Attachments are disabled"),
            Some(limit_kb) => {
                let already_used = Attachment::size_by_user(user_id, &mut conn).await;
                let left = limit_kb
                    .checked_mul(1024)
                    .and_then(|l| l.checked_sub(already_used))
                    .and_then(|l| l.checked_add(size_adjust));

                let Some(left) = left else {
                    err!("Attachment size overflow");
                };

                if left <= 0 {
                    err!("Attachment storage limit reached! Delete some attachments to free up space")
                }

                Some(left)
            }
            None => None,
        }
    } else if let Some(ref org_id) = cipher.organization_uuid {
        match CONFIG.org_attachment_limit() {
            Some(0) => err!("Attachments are disabled"),
            Some(limit_kb) => {
                let already_used = Attachment::size_by_org(org_id, &mut conn).await;
                let left = limit_kb
                    .checked_mul(1024)
                    .and_then(|l| l.checked_sub(already_used))
                    .and_then(|l| l.checked_add(size_adjust));

                let Some(left) = left else {
                    err!("Attachment size overflow");
                };

                if left <= 0 {
                    err!("Attachment storage limit reached! Delete some attachments to free up space")
                }

                Some(left)
            }
            None => None,
        }
    } else {
        err!("Cipher is neither owned by a user nor an organization");
    };

    if let Some(size_limit) = size_limit {
        if size > size_limit {
            err!("Attachment storage limit exceeded with this file");
        }
    }

    let file_id = match &attachment {
        Some(attachment) => attachment.id.clone(), // v2 API
        None => crypto::generate_attachment_id(),  // Legacy API
    };

    if let Some(attachment) = &mut attachment {
        // v2 API

        // Check the actual size against the size initially provided by
        // the client. Upstream allows +/- 1 MiB deviation from this
        // size, but it's not clear when or why this is needed.
        const LEEWAY: i64 = 1024 * 1024; // 1 MiB
        let Some(max_size) = attachment.file_size.checked_add(LEEWAY) else {
            err!("Invalid attachment size max")
        };
        let Some(min_size) = attachment.file_size.checked_sub(LEEWAY) else {
            err!("Invalid attachment size min")
        };

        if min_size <= size && size <= max_size {
            if size != attachment.file_size {
                // Update the attachment with the actual file size.
                attachment.file_size = size;
                attachment.save(&mut conn).await.expect("Error updating attachment");
            }
        } else {
            attachment.delete(&mut conn).await.ok();

            err!(format!("Attachment size mismatch (expected within [{min_size}, {max_size}], got {size})"));
        }
    } else {
        // Legacy API

        // SAFETY: This value is only stored in the database and is not used to access the file system.
        // As a result, the conditions specified by Rocket [0] are met and this is safe to use.
        // [0]: https://docs.rs/rocket/latest/rocket/fs/struct.FileName.html#-danger-
        let encrypted_filename = data.data.raw_name().map(|s| s.dangerous_unsafe_unsanitized_raw().to_string());

        if encrypted_filename.is_none() {
            err!("No filename provided")
        }
        if data.key.is_none() {
            err!("No attachment key provided")
        }
        let attachment =
            Attachment::new(file_id.clone(), cipher_id.clone(), encrypted_filename.unwrap(), size, data.key);
        attachment.save(&mut conn).await.expect("Error saving attachment");
    }

    save_temp_file(PathType::Attachments, &format!("{cipher_id}/{file_id}"), data.data, true).await?;

    nt.send_cipher_update(
        UpdateType::SyncCipherUpdate,
        &cipher,
        &cipher.update_users_revision(&mut conn).await,
        &headers.device,
        None,
        &mut conn,
    )
    .await;

    if let Some(org_id) = &cipher.organization_uuid {
        log_event(
            EventType::CipherAttachmentCreated as i32,
            &cipher.uuid,
            org_id,
            &headers.user.uuid,
            headers.device.atype,
            &headers.ip.ip,
            &mut conn,
        )
        .await;
    }

    Ok((cipher, conn))
}

/// v2 API for uploading the actual data content of an attachment.
/// This route needs a rank specified so that Rocket prioritizes the
/// /ciphers/<cipher_id>/attachment/v2 route, which would otherwise conflict
/// with this one.
#[post("/ciphers/<cipher_id>/attachment/<attachment_id>", format = "multipart/form-data", data = "<data>", rank = 1)]
async fn post_attachment_v2_data(
    cipher_id: CipherId,
    attachment_id: AttachmentId,
    data: Form<UploadData<'_>>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    let attachment = match Attachment::find_by_id(&attachment_id, &mut conn).await {
        Some(attachment) if cipher_id == attachment.cipher_uuid => Some(attachment),
        Some(_) => err!("Attachment doesn't belong to cipher"),
        None => err!("Attachment doesn't exist"),
    };

    save_attachment(attachment, cipher_id, data, &headers, conn, nt).await?;

    Ok(())
}

/// Legacy API for creating an attachment associated with a cipher.
#[post("/ciphers/<cipher_id>/attachment", format = "multipart/form-data", data = "<data>")]
async fn post_attachment(
    cipher_id: CipherId,
    data: Form<UploadData<'_>>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    // Setting this as None signifies to save_attachment() that it should create
    // the attachment database record as well as saving the data to disk.
    let attachment = None;

    let (cipher, mut conn) = save_attachment(attachment, cipher_id, data, &headers, conn, nt).await?;

    Ok(Json(cipher.to_json(&headers.host, &headers.user.uuid, None, CipherSyncType::User, &mut conn).await?))
}

#[post("/ciphers/<cipher_id>/attachment-admin", format = "multipart/form-data", data = "<data>")]
async fn post_attachment_admin(
    cipher_id: CipherId,
    data: Form<UploadData<'_>>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    post_attachment(cipher_id, data, headers, conn, nt).await
}

#[post("/ciphers/<cipher_id>/attachment/<attachment_id>/share", format = "multipart/form-data", data = "<data>")]
async fn post_attachment_share(
    cipher_id: CipherId,
    attachment_id: AttachmentId,
    data: Form<UploadData<'_>>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    _delete_cipher_attachment_by_id(&cipher_id, &attachment_id, &headers, &mut conn, &nt).await?;
    post_attachment(cipher_id, data, headers, conn, nt).await
}

#[post("/ciphers/<cipher_id>/attachment/<attachment_id>/delete-admin")]
async fn delete_attachment_post_admin(
    cipher_id: CipherId,
    attachment_id: AttachmentId,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    delete_attachment(cipher_id, attachment_id, headers, conn, nt).await
}

#[post("/ciphers/<cipher_id>/attachment/<attachment_id>/delete")]
async fn delete_attachment_post(
    cipher_id: CipherId,
    attachment_id: AttachmentId,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    delete_attachment(cipher_id, attachment_id, headers, conn, nt).await
}

#[delete("/ciphers/<cipher_id>/attachment/<attachment_id>")]
async fn delete_attachment(
    cipher_id: CipherId,
    attachment_id: AttachmentId,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    _delete_cipher_attachment_by_id(&cipher_id, &attachment_id, &headers, &mut conn, &nt).await
}

#[delete("/ciphers/<cipher_id>/attachment/<attachment_id>/admin")]
async fn delete_attachment_admin(
    cipher_id: CipherId,
    attachment_id: AttachmentId,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    _delete_cipher_attachment_by_id(&cipher_id, &attachment_id, &headers, &mut conn, &nt).await
}

#[post("/ciphers/<cipher_id>/delete")]
async fn delete_cipher_post(cipher_id: CipherId, headers: Headers, mut conn: DbConn, nt: Notify<'_>) -> EmptyResult {
    _delete_cipher_by_uuid(&cipher_id, &headers, &mut conn, false, &nt).await
    // permanent delete
}

#[post("/ciphers/<cipher_id>/delete-admin")]
async fn delete_cipher_post_admin(
    cipher_id: CipherId,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    _delete_cipher_by_uuid(&cipher_id, &headers, &mut conn, false, &nt).await
    // permanent delete
}

#[put("/ciphers/<cipher_id>/delete")]
async fn delete_cipher_put(cipher_id: CipherId, headers: Headers, mut conn: DbConn, nt: Notify<'_>) -> EmptyResult {
    _delete_cipher_by_uuid(&cipher_id, &headers, &mut conn, true, &nt).await
    // soft delete
}

#[put("/ciphers/<cipher_id>/delete-admin")]
async fn delete_cipher_put_admin(
    cipher_id: CipherId,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    _delete_cipher_by_uuid(&cipher_id, &headers, &mut conn, true, &nt).await
}

#[delete("/ciphers/<cipher_id>")]
async fn delete_cipher(cipher_id: CipherId, headers: Headers, mut conn: DbConn, nt: Notify<'_>) -> EmptyResult {
    _delete_cipher_by_uuid(&cipher_id, &headers, &mut conn, false, &nt).await
    // permanent delete
}

#[delete("/ciphers/<cipher_id>/admin")]
async fn delete_cipher_admin(cipher_id: CipherId, headers: Headers, mut conn: DbConn, nt: Notify<'_>) -> EmptyResult {
    _delete_cipher_by_uuid(&cipher_id, &headers, &mut conn, false, &nt).await
    // permanent delete
}

#[delete("/ciphers", data = "<data>")]
async fn delete_cipher_selected(
    data: Json<CipherIdsData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    _delete_multiple_ciphers(data, headers, conn, false, nt).await // permanent delete
}

#[post("/ciphers/delete", data = "<data>")]
async fn delete_cipher_selected_post(
    data: Json<CipherIdsData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    _delete_multiple_ciphers(data, headers, conn, false, nt).await // permanent delete
}

#[put("/ciphers/delete", data = "<data>")]
async fn delete_cipher_selected_put(
    data: Json<CipherIdsData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    _delete_multiple_ciphers(data, headers, conn, true, nt).await // soft delete
}

#[delete("/ciphers/admin", data = "<data>")]
async fn delete_cipher_selected_admin(
    data: Json<CipherIdsData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    _delete_multiple_ciphers(data, headers, conn, false, nt).await // permanent delete
}

#[post("/ciphers/delete-admin", data = "<data>")]
async fn delete_cipher_selected_post_admin(
    data: Json<CipherIdsData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    _delete_multiple_ciphers(data, headers, conn, false, nt).await // permanent delete
}

#[put("/ciphers/delete-admin", data = "<data>")]
async fn delete_cipher_selected_put_admin(
    data: Json<CipherIdsData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    _delete_multiple_ciphers(data, headers, conn, true, nt).await // soft delete
}

#[put("/ciphers/<cipher_id>/restore")]
async fn restore_cipher_put(cipher_id: CipherId, headers: Headers, mut conn: DbConn, nt: Notify<'_>) -> JsonResult {
    _restore_cipher_by_uuid(&cipher_id, &headers, &mut conn, &nt).await
}

#[put("/ciphers/<cipher_id>/restore-admin")]
async fn restore_cipher_put_admin(
    cipher_id: CipherId,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    _restore_cipher_by_uuid(&cipher_id, &headers, &mut conn, &nt).await
}

#[put("/ciphers/restore", data = "<data>")]
async fn restore_cipher_selected(
    data: Json<CipherIdsData>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> JsonResult {
    _restore_multiple_ciphers(data, &headers, &mut conn, &nt).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveCipherData {
    folder_id: Option<FolderId>,
    ids: Vec<CipherId>,
}

#[post("/ciphers/move", data = "<data>")]
async fn move_cipher_selected(
    data: Json<MoveCipherData>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    let data = data.into_inner();
    let user_id = headers.user.uuid;

    if let Some(ref folder_id) = data.folder_id {
        if Folder::find_by_uuid_and_user(folder_id, &user_id, &mut conn).await.is_none() {
            err!("Invalid folder", "Folder does not exist or belongs to another user");
        }
    }

    for cipher_id in data.ids {
        let Some(cipher) = Cipher::find_by_uuid(&cipher_id, &mut conn).await else {
            err!("Cipher doesn't exist")
        };

        if !cipher.is_accessible_to_user(&user_id, &mut conn).await {
            err!("Cipher is not accessible by user")
        }

        // Move cipher
        cipher.move_to_folder(data.folder_id.clone(), &user_id, &mut conn).await?;

        nt.send_cipher_update(
            UpdateType::SyncCipherUpdate,
            &cipher,
            std::slice::from_ref(&user_id),
            &headers.device,
            None,
            &mut conn,
        )
        .await;
    }

    Ok(())
}

#[put("/ciphers/move", data = "<data>")]
async fn move_cipher_selected_put(
    data: Json<MoveCipherData>,
    headers: Headers,
    conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    move_cipher_selected(data, headers, conn, nt).await
}

#[derive(FromForm)]
struct OrganizationIdData {
    #[field(name = "organizationId")]
    org_id: OrganizationId,
}

#[post("/ciphers/purge?<organization..>", data = "<data>")]
async fn delete_all(
    organization: Option<OrganizationIdData>,
    data: Json<PasswordOrOtpData>,
    headers: Headers,
    mut conn: DbConn,
    nt: Notify<'_>,
) -> EmptyResult {
    let data: PasswordOrOtpData = data.into_inner();
    let mut user = headers.user;

    data.validate(&user, true, &mut conn).await?;

    match organization {
        Some(org_data) => {
            // Organization ID in query params, purging organization vault
            match Membership::find_by_user_and_org(&user.uuid, &org_data.org_id, &mut conn).await {
                None => err!("You don't have permission to purge the organization vault"),
                Some(member) => {
                    if member.atype == MembershipType::Owner {
                        Cipher::delete_all_by_organization(&org_data.org_id, &mut conn).await?;
                        nt.send_user_update(UpdateType::SyncVault, &user, &headers.device.push_uuid, &mut conn).await;

                        log_event(
                            EventType::OrganizationPurgedVault as i32,
                            &org_data.org_id,
                            &org_data.org_id,
                            &user.uuid,
                            headers.device.atype,
                            &headers.ip.ip,
                            &mut conn,
                        )
                        .await;

                        Ok(())
                    } else {
                        err!("You don't have permission to purge the organization vault");
                    }
                }
            }
        }
        None => {
            // No organization ID in query params, purging user vault
            // Delete ciphers and their attachments
            for cipher in Cipher::find_owned_by_user(&user.uuid, &mut conn).await {
                cipher.delete(&mut conn).await?;
            }

            // Delete folders
            for f in Folder::find_by_user(&user.uuid, &mut conn).await {
                f.delete(&mut conn).await?;
            }

            user.update_revision(&mut conn).await?;
            nt.send_user_update(UpdateType::SyncVault, &user, &headers.device.push_uuid, &mut conn).await;

            Ok(())
        }
    }
}

async fn _delete_cipher_by_uuid(
    cipher_id: &CipherId,
    headers: &Headers,
    conn: &mut DbConn,
    soft_delete: bool,
    nt: &Notify<'_>,
) -> EmptyResult {
    let Some(mut cipher) = Cipher::find_by_uuid(cipher_id, conn).await else {
        err!("Cipher doesn't exist")
    };

    if !cipher.is_write_accessible_to_user(&headers.user.uuid, conn).await {
        err!("Cipher can't be deleted by user")
    }

    if soft_delete {
        cipher.deleted_at = Some(Utc::now().naive_utc());
        cipher.save(conn).await?;
        nt.send_cipher_update(
            UpdateType::SyncCipherUpdate,
            &cipher,
            &cipher.update_users_revision(conn).await,
            &headers.device,
            None,
            conn,
        )
        .await;
    } else {
        cipher.delete(conn).await?;
        nt.send_cipher_update(
            UpdateType::SyncCipherDelete,
            &cipher,
            &cipher.update_users_revision(conn).await,
            &headers.device,
            None,
            conn,
        )
        .await;
    }

    if let Some(org_id) = cipher.organization_uuid {
        let event_type = match soft_delete {
            true => EventType::CipherSoftDeleted as i32,
            false => EventType::CipherDeleted as i32,
        };

        log_event(event_type, &cipher.uuid, &org_id, &headers.user.uuid, headers.device.atype, &headers.ip.ip, conn)
            .await;
    }

    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CipherIdsData {
    ids: Vec<CipherId>,
}

async fn _delete_multiple_ciphers(
    data: Json<CipherIdsData>,
    headers: Headers,
    mut conn: DbConn,
    soft_delete: bool,
    nt: Notify<'_>,
) -> EmptyResult {
    let data = data.into_inner();

    for cipher_id in data.ids {
        if let error @ Err(_) = _delete_cipher_by_uuid(&cipher_id, &headers, &mut conn, soft_delete, &nt).await {
            return error;
        };
    }

    Ok(())
}

async fn _restore_cipher_by_uuid(
    cipher_id: &CipherId,
    headers: &Headers,
    conn: &mut DbConn,
    nt: &Notify<'_>,
) -> JsonResult {
    let Some(mut cipher) = Cipher::find_by_uuid(cipher_id, conn).await else {
        err!("Cipher doesn't exist")
    };

    if !cipher.is_write_accessible_to_user(&headers.user.uuid, conn).await {
        err!("Cipher can't be restored by user")
    }

    cipher.deleted_at = None;
    cipher.save(conn).await?;

    nt.send_cipher_update(
        UpdateType::SyncCipherUpdate,
        &cipher,
        &cipher.update_users_revision(conn).await,
        &headers.device,
        None,
        conn,
    )
    .await;

    if let Some(org_id) = &cipher.organization_uuid {
        log_event(
            EventType::CipherRestored as i32,
            &cipher.uuid.clone(),
            org_id,
            &headers.user.uuid,
            headers.device.atype,
            &headers.ip.ip,
            conn,
        )
        .await;
    }

    Ok(Json(cipher.to_json(&headers.host, &headers.user.uuid, None, CipherSyncType::User, conn).await?))
}

async fn _restore_multiple_ciphers(
    data: Json<CipherIdsData>,
    headers: &Headers,
    conn: &mut DbConn,
    nt: &Notify<'_>,
) -> JsonResult {
    let data = data.into_inner();

    let mut ciphers: Vec<Value> = Vec::new();
    for cipher_id in data.ids {
        match _restore_cipher_by_uuid(&cipher_id, headers, conn, nt).await {
            Ok(json) => ciphers.push(json.into_inner()),
            err => return err,
        }
    }

    Ok(Json(json!({
      "data": ciphers,
      "object": "list",
      "continuationToken": null
    })))
}

async fn _delete_cipher_attachment_by_id(
    cipher_id: &CipherId,
    attachment_id: &AttachmentId,
    headers: &Headers,
    conn: &mut DbConn,
    nt: &Notify<'_>,
) -> JsonResult {
    let Some(attachment) = Attachment::find_by_id(attachment_id, conn).await else {
        err!("Attachment doesn't exist")
    };

    if &attachment.cipher_uuid != cipher_id {
        err!("Attachment from other cipher")
    }

    let Some(cipher) = Cipher::find_by_uuid(cipher_id, conn).await else {
        err!("Cipher doesn't exist")
    };

    if !cipher.is_write_accessible_to_user(&headers.user.uuid, conn).await {
        err!("Cipher cannot be deleted by user")
    }

    // Delete attachment
    attachment.delete(conn).await?;
    nt.send_cipher_update(
        UpdateType::SyncCipherUpdate,
        &cipher,
        &cipher.update_users_revision(conn).await,
        &headers.device,
        None,
        conn,
    )
    .await;

    if let Some(ref org_id) = cipher.organization_uuid {
        log_event(
            EventType::CipherAttachmentDeleted as i32,
            &cipher.uuid,
            org_id,
            &headers.user.uuid,
            headers.device.atype,
            &headers.ip.ip,
            conn,
        )
        .await;
    }
    let cipher_json = cipher.to_json(&headers.host, &headers.user.uuid, None, CipherSyncType::User, conn).await?;
    Ok(Json(json!({"cipher":cipher_json})))
}

/// This will hold all the necessary data to improve a full sync of all the ciphers
/// It can be used during the `Cipher::to_json()` call.
/// It will prevent the so called N+1 SQL issue by running just a few queries which will hold all the data needed.
/// This will not improve the speed of a single cipher.to_json() call that much, so better not to use it for those calls.
pub struct CipherSyncData {
    pub cipher_attachments: HashMap<CipherId, Vec<Attachment>>,
    pub cipher_folders: HashMap<CipherId, FolderId>,
    pub cipher_favorites: HashSet<CipherId>,
    pub cipher_collections: HashMap<CipherId, Vec<CollectionId>>,
    pub members: HashMap<OrganizationId, Membership>,
    pub user_collections: HashMap<CollectionId, CollectionUser>,
    pub user_collections_groups: HashMap<CollectionId, CollectionGroup>,
    pub user_group_full_access_for_organizations: HashSet<OrganizationId>,
}

#[derive(Eq, PartialEq)]
pub enum CipherSyncType {
    User,
    Organization,
}

impl CipherSyncData {
    pub async fn new(user_id: &UserId, sync_type: CipherSyncType, conn: &mut DbConn) -> Self {
        let cipher_folders: HashMap<CipherId, FolderId>;
        let cipher_favorites: HashSet<CipherId>;
        match sync_type {
            // User Sync supports Folders and Favorites
            CipherSyncType::User => {
                // Generate a HashMap with the Cipher UUID as key and the Folder UUID as value
                cipher_folders = FolderCipher::find_by_user(user_id, conn).await.into_iter().collect();

                // Generate a HashSet of all the Cipher UUID's which are marked as favorite
                cipher_favorites = Favorite::get_all_cipher_uuid_by_user(user_id, conn).await.into_iter().collect();
            }
            // Organization Sync does not support Folders and Favorites.
            // If these are set, it will cause issues in the web-vault.
            CipherSyncType::Organization => {
                cipher_folders = HashMap::with_capacity(0);
                cipher_favorites = HashSet::with_capacity(0);
            }
        }

        // Generate a list of Cipher UUID's containing a Vec with one or more Attachment records
        let orgs = Membership::get_orgs_by_user(user_id, conn).await;
        let attachments = Attachment::find_all_by_user_and_orgs(user_id, &orgs, conn).await;
        let mut cipher_attachments: HashMap<CipherId, Vec<Attachment>> = HashMap::with_capacity(attachments.len());
        for attachment in attachments {
            cipher_attachments.entry(attachment.cipher_uuid.clone()).or_default().push(attachment);
        }

        // Generate a HashMap with the Cipher UUID as key and one or more Collection UUID's
        let user_cipher_collections = Cipher::get_collections_with_cipher_by_user(user_id.clone(), conn).await;
        let mut cipher_collections: HashMap<CipherId, Vec<CollectionId>> =
            HashMap::with_capacity(user_cipher_collections.len());
        for (cipher, collection) in user_cipher_collections {
            cipher_collections.entry(cipher).or_default().push(collection);
        }

        // Generate a HashMap with the Organization UUID as key and the Membership record
        let members: HashMap<OrganizationId, Membership> =
            Membership::find_by_user(user_id, conn).await.into_iter().map(|m| (m.org_uuid.clone(), m)).collect();

        // Generate a HashMap with the User_Collections UUID as key and the CollectionUser record
        let user_collections: HashMap<CollectionId, CollectionUser> = CollectionUser::find_by_user(user_id, conn)
            .await
            .into_iter()
            .map(|uc| (uc.collection_uuid.clone(), uc))
            .collect();

        // Generate a HashMap with the collections_uuid as key and the CollectionGroup record
        let user_collections_groups: HashMap<CollectionId, CollectionGroup> = if CONFIG.org_groups_enabled() {
            CollectionGroup::find_by_user(user_id, conn).await.into_iter().fold(
                HashMap::new(),
                |mut combined_permissions, cg| {
                    combined_permissions
                        .entry(cg.collections_uuid.clone())
                        .and_modify(|existing| {
                            // Combine permissions: take the most permissive settings.
                            existing.read_only &= cg.read_only; // false if ANY group allows write
                            existing.hide_passwords &= cg.hide_passwords; // false if ANY group allows password view
                            existing.manage |= cg.manage; // true if ANY group allows manage
                        })
                        .or_insert(cg);
                    combined_permissions
                },
            )
        } else {
            HashMap::new()
        };

        // Get all organizations that the given user has full access to via group assignment
        let user_group_full_access_for_organizations: HashSet<OrganizationId> = if CONFIG.org_groups_enabled() {
            Group::get_orgs_by_user_with_full_access(user_id, conn).await.into_iter().collect()
        } else {
            HashSet::new()
        };

        Self {
            cipher_attachments,
            cipher_folders,
            cipher_favorites,
            cipher_collections,
            members,
            user_collections,
            user_collections_groups,
            user_group_full_access_for_organizations,
        }
    }
}
