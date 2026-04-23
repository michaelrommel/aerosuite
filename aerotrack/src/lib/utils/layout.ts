/**
 * Map an agent_index (0–99) to a 10×10 CSS-grid position.
 *
 * row = floor(agent_index / 10)
 * col = agent_index % 10
 */
export function agentGridPos(index: number): { row: number; col: number } {
	return {
		row: Math.floor(index / 10),
		col: index % 10,
	};
}
