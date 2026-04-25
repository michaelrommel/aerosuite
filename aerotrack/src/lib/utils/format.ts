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
 * Auto-scale bytes-per-second to a human-readable IEC binary bit-rate string.
 * Uses powers of 1024 and IEC unit names (GiBit/s, MiBit/s, KiBit/s).
 */
export function formatBandwidth(bps: number): string {
	const bits    = bps * 8;
	const GiBit   = 1024 ** 3;
	const MiBit   = 1024 ** 2;
	const KiBit   = 1024;
	if (bits >= GiBit) return `${(bits / GiBit).toFixed(2)} GiBit/s`;
	if (bits >= MiBit) return `${(bits / MiBit).toFixed(2)} MiBit/s`;
	if (bits >= KiBit) return `${(bits / KiBit).toFixed(2)} KiBit/s`;
	return                    `${bits.toFixed(0)} bit/s`;
}
