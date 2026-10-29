import type { CSSProperties } from 'react';

/** The control's own background fills with progress; the label is repeated on
 *  the fill in the fill's text colour so it reads over both parts. */
export function ProgressFill({value,children}:{value:number;children:string}) {
  return <>{children}<span className="upgrade-progress" aria-hidden="true" style={{'--upgrade-progress':`${value}%`} as CSSProperties}>{children}</span></>;
}
