import { DiagnosticsChart, DEFAULT_SPAN } from './LatencyChart';
import type { ChartSpan } from './LatencyChart';
export function AudioQualityChart({active, span = DEFAULT_SPAN}: {active:boolean; span?: ChartSpan}) {
    return <DiagnosticsChart active={active} audio span={span}/>;
}
