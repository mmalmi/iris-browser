import animals from './data/animals.json' with { type: 'json' };
import adjectives from './data/adjectives.json' with { type: 'json' };

function capitalize(value: string): string {
  if (typeof value !== 'string') return '';
  return value.charAt(0).toUpperCase() + value.slice(1);
}

function simpleHash(seed: string): [number, number] {
  let h1 = 0;
  let h2 = 0;
  for (let index = 0; index < seed.length; index += 1) {
    const code = seed.charCodeAt(index);
    h1 = (h1 * 31 + code) >>> 0;
    h2 = (h2 * 37 + code) >>> 0;
  }
  return [h1 & 0xff, h2 & 0xff];
}

export function animalName(seed: string): string {
  if (!seed) {
    throw new Error('No seed provided');
  }
  const [h1, h2] = simpleHash(seed);
  const adjective = adjectives[h1 % adjectives.length];
  const animal = animals[h2 % animals.length];
  return `${capitalize(adjective)} ${capitalize(animal)}`;
}
