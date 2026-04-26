// Shared TypeScript types matching the aerocoach JSON wire format.

export interface TransferRecord {
	agent_id: string;
	filename: string;
	bucket_id: string;
	bytes_transferred: number;
	file_size_bytes: number;
	bandwidth_kibps: number;
	success: boolean;
	error_reason?: string;
	start_time_ms: number;
	end_time_ms: number;
	time_slice: number;
}

export interface AgentSnapshot {
	agent_id: string;
	agent_index: number;
	connected: boolean;
	current_slice: number;
	active_connections: number;
	bytes_transferred: number;
	bytes_in_flight: number;
	success_count: number;
	error_count: number;
	private_ip: string;
	instance_id: string;
	/** True once the agent has sent a PlanAck after the last Confirm. */
	plan_acked: boolean;
}

export interface GlobalStats {
	total_bytes_transferred: number;
	total_success: number;
	total_errors: number;
	active_agents: number;
	active_connections: number;
	overall_error_rate: number;
	current_bandwidth_bps: number;
}

export interface DashboardUpdate {
	timestamp_ms: number;
	current_slice: number;
	total_slices: number;
	agents: AgentSnapshot[];
	completed_transfers: TransferRecord[];
	global_stats: GlobalStats;
}

export interface SliceSpec {
	slice_index: number;
	total_connections: number;
}

export interface BucketSpec {
	bucket_id: string;
	size_min_bytes: number;
	size_max_bytes: number;
	percentage: number;
}

export interface FileDistribution {
	buckets: BucketSpec[];
}

export interface LoadPlan {
	plan_id: string;
	slice_duration_ms: number;
	total_bandwidth_bps: number;
	file_distribution: FileDistribution;
	slices: SliceSpec[];
}

/** One entry returned by `GET /plans`. */
export interface PlanEntry {
	filename: string;
	plan_id:  string;
}
