import type { ComponentProps } from 'react';
import { Button, Row } from 'oink-ui';
import './permissions.css';

export function PermissionRow({ className = '', ...props }: ComponentProps<typeof Row>) {
  return <Row {...props} className={`permission-row ${className}`} />;
}

export function PermissionButton({ className = '', ...props }: ComponentProps<typeof Button>) {
  return <Button {...props} size="sm" className={`permission-button ${className}`} />;
}
