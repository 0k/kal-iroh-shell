import type { Plugin } from 'vite';

export const MANIFEST_FILE: string;
export const POINTER_FILE: string;
export const KEPT_RELEASES: number;
export interface ManifestEntry { path: string; size: number; sha256: string }
export interface UiManifest { build_id: string; files: ManifestEntry[] }
export interface UiPointer { build_id: string; published_at: string }
export function newBuildId(now?: Date): string;
export function buildManifest(dist: string, buildId: string): UiManifest;
export function writeManifest(dist: string, buildId: string): UiManifest;
export function readPointer(publishDir: string): UiPointer | null;
export function publishRelease(dist: string, publishDir: string, now?: Date): string;
export function pruneReleases(publishDir: string, keep: number): string[];
export function kalIrohShellRelease(options?: { publishDir?: string }): Plugin;
