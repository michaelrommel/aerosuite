/**
 * Auto-scale a byte count to a human-readable string.
 */
export function formatBytes(bytes: number): string {
	if (bytes < 1_024)           return `${bytes} B`;
	if (bytes < 1_024 ** 2)      return `${(bytes / 1_024).toFixed(1)} KB`;
	if (bytes < 1_024 ** 3)      return `${(bytes / 1_024 ** 2).toFixed(2)} MB`;
	return                              `${(bytes / 1_024 ** 3).toFixed(2)} GB`;
}

/**
 * Auto-scale bytes-per-second to a Mbit/s or Gbit/s string.
 */
export function formatBandwidth(bps: number): string {
	const mbps = (bps * 8) / 1_000_000;
	if (mbps < 0.01)  return '< 0.01 Mbit/s';
	if (mbps < 1_000) return `${mbps.toFixed(2)} Mbit/s`;
	return                   `${(mbps / 1_000).toFixed(2)} Gbit/s`;
}
