import { invoke, isTauri } from '@tauri-apps/api/core';
import type { SnapEdges } from './geometry';

/** The native snap bridge shared by layout alignment and pointer volume detents. */
export function alignmentHaptic() {
    if (isTauri()) {
        // Feedback is optional; shutdown must not produce an unhandled rejection.
        void invoke('shell_alignment_haptic').catch(() => {});
    }
}

export class SnapFeedback {
    private edges: SnapEdges = {};
    private lastPulse = -Infinity;

    reset(edges: SnapEdges = {}) { this.edges = edges; }

    update(edges: SnapEdges) {
        const engaged = (['x', 'y'] as const).some(axis => edges[axis] !== undefined && edges[axis] !== this.edges[axis]);
        this.edges = edges;
        if (engaged) this.pulse();
    }

    pulse() {
        const now = performance.now();
        if (now - this.lastPulse < 100) return;
        this.lastPulse = now;
        alignmentHaptic();
    }
}
