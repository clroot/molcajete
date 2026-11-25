/** The molcajete mark (public/favicon.svg). */
export default function Logo({size = 24}: {size?: number}) {
  return <img src="./favicon.svg" width={size} height={size} alt="" aria-hidden="true" />;
}
