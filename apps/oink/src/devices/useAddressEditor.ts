import { useRef, useState } from 'react';
import { api } from '../api/client';
import { toFailure } from '../api/transport';
import { problemText } from '../i18n';
import type { Device } from './types';
import { DEFAULT_PORT } from '../pairing/data';

export function useAddressEditor() {
  const [device, setDevice] = useState<Device | null>(null);
  const [addresses, setAddresses] = useState<string[]>([]);
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const generation = useRef(0);
  const cancel = () => { generation.current++; setDevice(null); };
  const open = async (target: Device) => {
    const current = ++generation.current;
    setDevice({...target,port:target.port || DEFAULT_PORT}); setAddresses([]); setProblem(null); setPending(true);
    try {
      const stored = await api.memberAddresses(target.id);
      if (generation.current !== current) return;
      setAddresses(stored);
      const address = stored.find(a => /^\d+\.\d+\.\d+\.\d+:\d+$/.test(a));
      if (address) {
        const [ip, port] = address.split(':');
        setDevice({...target, ip, port: Number(port)});
      }
    } catch (error) {
      if (generation.current === current) setProblem(problemText(toFailure(error).problem.code));
    } finally {
      if (generation.current === current) setPending(false);
    }
  };
  const save = async (target: Device, ip: string, port: number) => {
    const current = ++generation.current;
    setPending(true); setProblem(null);
    try {
      await api.setMemberAddress(target.id, `${ip}:${port}`);
      if (generation.current === current) setDevice(null);
    } catch (error) {
      if (generation.current === current) setProblem(problemText(toFailure(error).problem.code));
    } finally {
      if (generation.current === current) setPending(false);
    }
  };
  return {device, addresses, pending, problem, open, cancel, save};
}
