'use client';

// React Bits Star Border by David Haz. Source/license: licenses/React-Bits.txt.
// Local additions: optional animated flag and aria-hidden decorative layers.
import React from 'react';
import './StarBorder.css';

type StarBorderProps<T extends React.ElementType> = React.ComponentPropsWithoutRef<T> & {
  as?: T;
  className?: string;
  children?: React.ReactNode;
  color?: string;
  speed?: React.CSSProperties['animationDuration'];
  thickness?: number;
  backgroundColor?: string;
  textColor?: string;
  borderColor?: string;
  animated?: boolean;
};

const StarBorder = <T extends React.ElementType = 'button'>({
  as,
  className = '',
  color = 'white',
  speed = '6s',
  thickness = 1,
  backgroundColor = '#000000',
  textColor = '#ffffff',
  borderColor = '#222222',
  animated = true,
  children,
  ...rest
}: StarBorderProps<T>) => {
  const Component = as || 'button';

  return (
    <Component
      className={`star-border-container ${className}`}
      {...(rest as any)}
      style={{ padding: `${thickness}px 0`, ...(rest as any).style }}
    >
      {animated && <>
        <div className="border-gradient-bottom" aria-hidden="true"
          style={{ background: `radial-gradient(circle, ${color}, transparent 10%)`, animationDuration: speed }} />
        <div className="border-gradient-top" aria-hidden="true"
          style={{ background: `radial-gradient(circle, ${color}, transparent 10%)`, animationDuration: speed }} />
      </>}
      <div className="inner-content" style={{ background: backgroundColor, color: textColor, borderColor }}>
        {children}
      </div>
    </Component>
  );
};

export default StarBorder;
