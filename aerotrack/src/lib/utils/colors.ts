/**
 * Error-rate colour palette — Gruvbox Dark.
 *
 * | Rate        | Colour         |
 * |-------------|----------------|
 * | 0 %         | bright-green   |
 * | >0 % – <1 % | bright-yellow  |
 * | 1 % – 3 %   | bright-orange  |
 * | >3 %        | bright-red     |
 */
export function errorRateColor(rate: number): string {
	if (rate === 0)   return '#b8bb26'; // bright green
	if (rate < 0.01)  return '#fabd2f'; // bright yellow
	if (rate <= 0.03) return '#fe8019'; // bright orange
	return                   '#fb4934'; // bright red
}
