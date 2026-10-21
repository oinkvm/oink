import { DiagnosticsChart } from './LatencyChart';
export function AudioQualityChart({active}: {active:boolean}) {
    return <DiagnosticsChart active={active} audio/>;
}
