import type {ReactNode} from 'react';
import {Banner} from '@astryxdesign/core/Banner';
import {Center} from '@astryxdesign/core/Center';
import {Spinner} from '@astryxdesign/core/Spinner';
import type {ApiData} from '../useApiData';

type Props<T> = {
  data: ApiData<T>;
  /** Banner title when loading fails, e.g. "Could not load users". */
  errorTitle: string;
  children: (data: T) => ReactNode;
};

/** A spinner while `data` loads, an error banner if it fails, then the content. */
export default function AsyncContent<T>({data, errorTitle, children}: Props<T>) {
  switch (data.status) {
    case 'loading':
      return (
        <Center axis="both" padding={10}>
          <Spinner size="lg" />
        </Center>
      );
    case 'error':
      return <Banner status="error" title={errorTitle} description={data.message} />;
    case 'ready':
      return <>{children(data.data)}</>;
  }
}
