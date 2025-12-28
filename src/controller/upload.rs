use super::{
    Claim, SiteConfig, User,
    db_utils::{IterType, u8_slice_to_u32},
    filters, incr_id,
    inn::ParamsTag,
    meta_handler::{PageData, get_referer, into_response},
    notification::{NtType, add_notification},
    u32_to_ivec,
    user::{InnRole, Role},
};
use crate::{DB, config::CONFIG, error::AppError};
use askama::Template;
use axum::{
    extract::{Multipart, Path, Query},
    response::{IntoResponse, Redirect},
};
use axum_extra::{
    TypedHeader,
    headers::{Cookie, Referer},
};
use data_encoding::HEXLOWER;
use image::ImageFormat;
use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};
use serde::Deserialize;

use tokio::fs::{self, remove_file};
use tracing::{error, warn};

#[derive(Deserialize)]
pub(crate) struct UploadPicParams {
    page_type: String,
    iid: Option<u32>,
}

/// `POST /mod/inn_icon` && `/user/avatar`
pub(crate) async fn upload_pic_post(
    cookie: Option<TypedHeader<Cookie>>,
    Query(params): Query<UploadPicParams>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, AppError> {
    let cookie = cookie.ok_or(AppError::NonLogin)?;
    let site_config = SiteConfig::get(&DB)?;
    let claim = Claim::get(&DB, &cookie, &site_config).ok_or(AppError::NonLogin)?;

    let target;
    let fname = match params.page_type.as_str() {
        "inn" => {
            if let Some(iid) = params.iid {
                let inn_role = InnRole::get(&DB, iid, claim.uid)?.ok_or(AppError::Unauthorized)?;
                if inn_role < InnRole::Mod {
                    return Err(AppError::Unauthorized);
                }
                target = format!("/mod/{iid}");
                format!("{}/{}.png", &CONFIG.inn_icons_path, iid)
            } else {
                return Err(AppError::NotFound);
            }
        }
        "user" => {
            target = "/user/setting".to_string();
            format!("{}/{}.png", &CONFIG.avatars_path, claim.uid)
        }
        _ => return Err(AppError::NotFound),
    };

    if let Some(field) = multipart.next_field().await.unwrap() {
        let data = match field.bytes().await {
            Ok(data) => data,
            Err(e) => {
                error!("{:?}", e);
                return Ok(e.into_response());
            }
        };
        let image_format_detected = image::guess_format(&data)?;
        image::load_from_memory_with_format(&data, image_format_detected)?;
        fs::write(fname, &data).await.unwrap();
    }

    Ok(Redirect::to(&target).into_response())
}

/// Page data: `gallery.html`
#[derive(Template)]
#[template(path = "gallery.html")]
struct PageGallery<'a> {
    page_data: PageData<'a>,
    imgs: Vec<(u32, String)>,
    anchor: usize,
    is_desc: bool,
    n: usize,
    uid: u32,
}

/// `GET /gallery/:uid`
pub(crate) async fn gallery(
    cookie: Option<TypedHeader<Cookie>>,
    Path(uid): Path<u32>,
    Query(params): Query<ParamsTag>,
) -> Result<impl IntoResponse, AppError> {
    let cookie = cookie.ok_or(AppError::NonLogin)?;
    let site_config = SiteConfig::get(&DB)?;
    let claim = Claim::get(&DB, &cookie, &site_config).ok_or(AppError::NonLogin)?;
    if claim.uid != uid && Role::from(claim.role) != Role::Admin {
        return Err(AppError::Unauthorized);
    }

    let has_unread = User::has_unread(&DB, claim.uid)?;

    let anchor = params.anchor.unwrap_or(0);
    let is_desc = params.is_desc.unwrap_or(true);
    let n = 12;

    let mut imgs = Vec::with_capacity(n);
    let ks = DB.open_partition("user_uploads", Default::default())?;
    let iter = ks.inner().prefix(u32_to_ivec(uid));
    let iter = if is_desc {
        IterType::Rev(iter.rev())
    } else {
        IterType::Fwd(iter)
    };

    for (idx, i) in iter.enumerate() {
        if idx < anchor {
            continue;
        }

        let (k, v) = i?;
        let img_id = u8_slice_to_u32(&k[4..]);
        let img = String::from_utf8_lossy(&v).to_string();
        imgs.push((img_id, img));

        if imgs.len() >= n {
            break;
        }
    }

    let page_data = PageData::new("gallery", &site_config, Some(claim), has_unread);
    let page_gallery = PageGallery {
        page_data,
        imgs,
        anchor,
        is_desc,
        n,
        uid,
    };

    Ok(into_response(&page_gallery))
}

/// `GET /image/delete/:uid/:img_id`
pub(crate) async fn image_delete(
    cookie: Option<TypedHeader<Cookie>>,
    Path((uid, img_id)): Path<(u32, u32)>,
    referer: Option<TypedHeader<Referer>>,
) -> Result<impl IntoResponse, AppError> {
    let cookie = cookie.ok_or(AppError::NonLogin)?;
    let site_config = SiteConfig::get(&DB)?;
    let claim = Claim::get(&DB, &cookie, &site_config).ok_or(AppError::NonLogin)?;

    if claim.uid != uid && Role::from(claim.role) != Role::Admin {
        return Err(AppError::Unauthorized);
    }

    let k = [u32_to_ivec(uid), u32_to_ivec(img_id)].concat();
    let tree = DB.open_partition("user_uploads", Default::default())?;
    if let Some(v1) = tree.take(&k)? {
        // When the same pictures uploaded, only one will be saved. So when deleting, we must check that.
        let mut count = 0;
        for i in tree.inner().iter() {
            let (_, v2) = i?;
            if v1 == v2 {
                count += 1;
                break;
            }
        }

        if count == 0 {
            let img = String::from_utf8_lossy(&v1);
            let path = format!("{}/{}", CONFIG.upload_path, img);
            remove_file(path).await?;
        }
    } else {
        return Err(AppError::NotFound);
    }

    if uid != claim.uid {
        add_notification(&DB, uid, NtType::ImageDelete, claim.uid, img_id)?;
    }

    let target = if let Some(referer) = get_referer(referer) {
        referer
    } else {
        format!("/gallery/{uid}")
    };
    Ok(Redirect::to(&target))
}

/// Page data: `upload.html`
#[derive(Template)]
#[template(path = "upload.html")]
struct PageUpload<'a> {
    page_data: PageData<'a>,
    imgs: Vec<String>,
    uid: u32,
}

/// `GET /upload`
pub(crate) async fn upload(
    cookie: Option<TypedHeader<Cookie>>,
) -> Result<impl IntoResponse, AppError> {
    let cookie = cookie.ok_or(AppError::NonLogin)?;
    let site_config = SiteConfig::get(&DB)?;
    let claim = Claim::get(&DB, &cookie, &site_config).ok_or(AppError::NonLogin)?;
    let has_unread = User::has_unread(&DB, claim.uid)?;
    let uid = claim.uid;
    let page_data = PageData::new("upload images", &site_config, Some(claim), has_unread);
    let page_upload = PageUpload {
        page_data,
        imgs: vec![],
        uid,
    };

    Ok(into_response(&page_upload))
}

/// `POST /upload`
pub(crate) async fn upload_post(
    cookie: Option<TypedHeader<Cookie>>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, AppError> {
    let cookie = cookie.ok_or(AppError::NonLogin)?;
    let site_config = SiteConfig::get(&DB)?;
    let claim = Claim::get(&DB, &cookie, &site_config).ok_or(AppError::NonLogin)?;

    let mut imgs = Vec::new();
    let mut batch = DB.inner().batch();
    let user_uploads = DB
        .inner()
        .open_partition("user_uploads", Default::default())?;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::Custom(e.to_string()))?
    {
        let data = match field.bytes().await {
            Ok(data) => data,
            Err(e) => {
                warn!("{:?}", e);
                continue; // @TODO frontend alert
            }
        };
        let format = match image::guess_format(&data) {
            Ok(format) => format,
            Err(e) => {
                warn!("{:?}", e);
                continue; // @TODO frontend alert
            }
        };
        if !matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
        ) {
            warn!("Unsupported image format: {:?}", format);
            continue;
        } // @TODO frontend alert
        match format.extensions_str().first() {
            Some(extension) => {
                let mut context = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
                context.update(&data);
                let filename = format!(
                    "{}.{extension}",
                    &HEXLOWER.encode(context.finish().as_ref()),
                );
                if let Err(e) =
                    fs::write(format!("{}/{filename}", &CONFIG.upload_path), &data).await
                {
                    error!("{:?}", e);
                    continue; // @TODO frontend alert
                }
                let img_id = incr_id(&DB, "imgs_count")?; // @TODO is this really work before the commit?
                let k = [u32_to_ivec(claim.uid), u32_to_ivec(img_id)].concat();
                batch.insert(&user_uploads, k, filename.as_bytes());

                imgs.push(filename)
            }
            None => warn!("Unsupported image extension"), // @TODO frontend alert
        }
    }

    batch.commit()?;

    let has_unread = User::has_unread(&DB, claim.uid)?;
    let uid = claim.uid;
    let page_data = PageData::new("upload images", &site_config, Some(claim), has_unread);
    let page_upload = PageUpload {
        page_data,
        imgs,
        uid,
    };

    Ok(into_response(&page_upload))
}
