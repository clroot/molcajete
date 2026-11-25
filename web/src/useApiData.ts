import {useCallback, useEffect, useState} from 'react';
import {errorMessage} from './api';

export type ApiData<T> =
  {status: 'loading'} | {status: 'error'; message: string} | {status: 'ready'; data: T};

/**
 * Loads `fetcher()` when it changes and on `reload()`. Responses that
 * arrive after a newer request (or after unmount) are dropped, so a slow
 * answer never overwrites a newer one.
 */
export function useApiData<T>(fetcher: () => Promise<T>): ApiData<T> & {reload: () => void} {
  const [state, setState] = useState<ApiData<T>>({status: 'loading'});
  const [generation, setGeneration] = useState(0);

  useEffect(() => {
    let isCurrent = true;
    fetcher().then(
      data => isCurrent && setState({status: 'ready', data}),
      err => isCurrent && setState({status: 'error', message: errorMessage(err)}),
    );
    return () => {
      isCurrent = false;
    };
  }, [fetcher, generation]);

  const reload = useCallback(() => setGeneration(g => g + 1), []);
  return {...state, reload};
}
