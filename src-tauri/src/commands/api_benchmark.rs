use crate::app_config::AppType;
use crate::error::AppError;
use crate::services::api_benchmark::{
    ApiBenchmarkEntry, ApiBenchmarkOptions, ApiBenchmarkProgressEmitter, ApiBenchmarkRunResult,
    ApiBenchmarkService, CodeEvaluationResult,
};
use crate::store::AppState;
use std::str::FromStr;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

fn api_benchmark_progress_emitter(
    app_handle: &AppHandle,
    options: &ApiBenchmarkOptions,
) -> Option<ApiBenchmarkProgressEmitter> {
    let _ = options.progress_run_id.as_ref()?;
    let app_handle = app_handle.clone();
    Some(Arc::new(move |event| {
        if let Err(err) = app_handle.emit("api-benchmark-progress", event) {
            log::debug!("failed to emit api benchmark progress event: {err}");
        }
    }))
}

#[tauri::command]
pub fn list_api_benchmark_entries(
    state: State<'_, AppState>,
    app: String,
) -> Result<Vec<ApiBenchmarkEntry>, AppError> {
    let app_type = AppType::from_str(&app).map_err(|e| AppError::Message(e.to_string()))?;
    let providers = state.db.get_all_providers(app_type.as_str())?;
    Ok(ApiBenchmarkService::list_entries(&app_type, &providers))
}

#[tauri::command]
pub async fn run_api_benchmark(
    app_handle: AppHandle,
    state: State<'_, AppState>,
    app: String,
    indices: Vec<usize>,
    options: ApiBenchmarkOptions,
) -> Result<ApiBenchmarkRunResult, AppError> {
    let app_type = AppType::from_str(&app).map_err(|e| AppError::Message(e.to_string()))?;
    let providers = state.db.get_all_providers(app_type.as_str())?;
    let targets = ApiBenchmarkService::resolve_targets(&app_type, &providers, &indices)?;
    let progress = api_benchmark_progress_emitter(&app_handle, &options);
    Ok(match progress {
        Some(progress) => {
            ApiBenchmarkService::run_targets_with_progress(targets, options, Some(progress)).await
        }
        None => ApiBenchmarkService::run_targets(targets, options).await,
    })
}

#[tauri::command]
pub async fn run_api_benchmark_provider(
    app_handle: AppHandle,
    state: State<'_, AppState>,
    app: String,
    provider_id: String,
    options: ApiBenchmarkOptions,
) -> Result<ApiBenchmarkRunResult, AppError> {
    let app_type = AppType::from_str(&app).map_err(|e| AppError::Message(e.to_string()))?;
    let providers = state.db.get_all_providers(app_type.as_str())?;
    let targets =
        ApiBenchmarkService::resolve_targets_for_provider(&app_type, &providers, &provider_id)?;
    let progress = api_benchmark_progress_emitter(&app_handle, &options);
    Ok(match progress {
        Some(progress) => {
            ApiBenchmarkService::run_targets_with_progress(targets, options, Some(progress)).await
        }
        None => ApiBenchmarkService::run_targets(targets, options).await,
    })
}

#[tauri::command]
pub fn update_api_benchmark_concurrency(
    run_id: String,
    max_concurrency: usize,
) -> Result<bool, AppError> {
    Ok(ApiBenchmarkService::update_concurrency(
        &run_id,
        max_concurrency,
    ))
}

#[tauri::command]
pub async fn rerun_api_benchmark_python(
    code: String,
    task_id: Option<String>,
) -> Result<CodeEvaluationResult, AppError> {
    Ok(ApiBenchmarkService::rerun_python(code, task_id).await)
}
