import { useState, useEffect, useCallback, useRef } from "react";
import { check, Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { invoke } from "@tauri-apps/api/core";

export interface UpdateState {
    checking: boolean;
    available: boolean;
    downloading: boolean;
    readyToInstall: boolean;
    progress: number;
    error: string | null;
    update: Update | null;
}

/**
 * Back up user data before an update installs.
 *
 * STUB. This template has no data worth backing up, so it resolves
 * immediately. It is kept as a named step because the ORDERING below is the
 * part worth copying, and the ordering only means something if this call can
 * fail:
 *
 *     backup -> abort the whole update if the backup failed -> kill sidecars -> install
 *
 * In a real app, replace the body with `await invoke("create_pre_update_backup")`
 * and implement that Tauri command so it:
 *
 *   - copies the SQLite database (and its -wal/-shm siblings, or checkpoints
 *     first) to a timestamped folder,
 *   - returns Err on ANY failure rather than logging and continuing.
 *
 * An update that proceeds after a failed backup is an update that can lose the
 * user's data with no way back. Throwing here is the whole point.
 */
async function createPreUpdateBackup(): Promise<void> {
    // Replace with: await invoke("create_pre_update_backup");
    return;
}

/**
 * useUpdater hook - manages the app update lifecycle.
 *
 * Checks for updates on mount, downloads in the background, and exposes
 * `installAndRestart` for the UI to call once the user agrees.
 *
 * Wiring this into a UI is left to you; the demo app in `ui/src/App.tsx` is
 * about crash recovery and deliberately does not render update prompts.
 *
 * Requires `plugins.updater` in tauri.conf.json with a real minisign pubkey and
 * endpoint (the template ships documented placeholders), plus
 * `bundle.createUpdaterArtifacts: true` and a TAURI_SIGNING_PRIVATE_KEY at
 * build time. Until you set those up, `check()` fails and lands in `error` —
 * which is the correct behaviour for an unconfigured template, not a bug.
 */
export function useUpdater() {
    const [state, setState] = useState<UpdateState>({
        checking: false,
        available: false,
        downloading: false,
        readyToInstall: false,
        progress: 0,
        error: null,
        update: null,
    });

    // Track if download has been initiated to prevent race conditions
    const downloadInitiatedRef = useRef(false);

    const checkForUpdates = useCallback(async () => {
        setState((s) => ({ ...s, checking: true, error: null }));

        try {
            const update = await check();

            if (update) {
                setState((s) => ({
                    ...s,
                    checking: false,
                    available: true,
                    update,
                }));
                return update;
            } else {
                setState((s) => ({ ...s, checking: false, available: false }));
                return null;
            }
        } catch (error) {
            const message =
                error instanceof Error ? error.message : "Update check failed";
            setState((s) => ({ ...s, checking: false, error: message }));
            return null;
        }
    }, []);

    const downloadUpdate = useCallback(async () => {
        if (!state.update) return;

        setState((s) => ({ ...s, downloading: true, progress: 0 }));

        try {
            let downloaded = 0;
            let contentLength = 0;

            await state.update.download((event) => {
                if (event.event === "Started") {
                    contentLength = event.data.contentLength ?? 0;
                } else if (event.event === "Progress") {
                    downloaded += event.data.chunkLength;
                    const progress =
                        contentLength > 0
                            ? (downloaded / contentLength) * 100
                            : 0;
                    setState((s) => ({ ...s, progress }));
                } else if (event.event === "Finished") {
                    setState((s) => ({
                        ...s,
                        downloading: false,
                        readyToInstall: true,
                        progress: 100,
                    }));
                }
            });
        } catch (error) {
            const message =
                error instanceof Error ? error.message : "Download failed";
            setState((s) => ({ ...s, downloading: false, error: message }));
        }
    }, [state.update]);

    const installAndRestart = useCallback(async () => {
        if (!state.update) return;

        // 1. Create backup before installing - abort if backup fails.
        try {
            await createPreUpdateBackup();
        } catch (backupError) {
            const message =
                backupError instanceof Error
                    ? `Backup failed: ${backupError.message}. Update aborted.`
                    : "Backup failed. Update aborted for safety.";
            setState((s) => ({ ...s, error: message }));
            return;
        }

        // 2. Kill every sidecar process before the NSIS installer runs.
        // The installer cannot overwrite py-sidecar.exe while it is running,
        // and the failure mode is a half-installed app. `prepare_for_update`
        // lives in src-tauri/src/lib.rs. A warning here is tolerable — the NSIS
        // PREUNINSTALL hook kills the same processes again as a second net.
        try {
            await invoke("prepare_for_update");
        } catch (e) {
            console.warn("prepare_for_update warning:", e);
        }

        // 3. Install, only now that the backup succeeded.
        try {
            // On Windows this hands off to the NSIS installer and exits the app.
            await state.update.install();

            // Rarely reached on Windows because of the above; kept for other
            // platforms and for the case where install() returns.
            await relaunch();
        } catch (error) {
            const message =
                error instanceof Error ? error.message : "Installation failed";
            setState((s) => ({ ...s, error: message }));
        }
    }, [state.update]);

    const dismissUpdate = useCallback(() => {
        setState((s) => ({
            ...s,
            available: false,
            readyToInstall: false,
            update: null,
        }));
    }, []);

    const retryDownload = useCallback(() => {
        setState((s) => ({ ...s, error: null }));
        downloadUpdate();
    }, [downloadUpdate]);

    // Check on mount (with delay for app initialization)
    useEffect(() => {
        const timer = setTimeout(() => checkForUpdates(), 2000);
        return () => clearTimeout(timer);
    }, [checkForUpdates]);

    // Auto-download when update available (with ref to prevent race conditions)
    useEffect(() => {
        if (
            state.available &&
            state.update &&
            !state.downloading &&
            !state.readyToInstall &&
            !state.error &&
            !downloadInitiatedRef.current
        ) {
            downloadInitiatedRef.current = true;
            downloadUpdate();
        }
    }, [
        state.available,
        state.update,
        state.downloading,
        state.readyToInstall,
        state.error,
        downloadUpdate,
    ]);

    // Reset download flag when a new update version is detected
    useEffect(() => {
        downloadInitiatedRef.current = false;
    }, [state.update?.version]);

    return {
        ...state,
        checkForUpdates,
        downloadUpdate,
        installAndRestart,
        dismissUpdate,
        retryDownload,
    };
}
